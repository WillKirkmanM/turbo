use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use notify::Event;
use radix_trie::{Trie, TrieCommon};
use thiserror::Error;
use tokio::{
    select,
    sync::{self, broadcast, mpsc, oneshot, watch},
    time::Instant,
};
use tracing::{debug, trace};
use turbopath::{AbsoluteSystemPathBuf, AnchoredSystemPath, AnchoredSystemPathBuf};
use turborepo_repository::discovery::DiscoveryResponse;
use turborepo_scm::{package_deps::GitHashes, Error as SCMError, SCM};

use crate::{globwatcher::GlobSet, package_watcher::DiscoveryData, NotifyError, OptionalWatch};

pub struct HashWatcher {
    _exit_tx: oneshot::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
    query_tx: mpsc::Sender<Query>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HashSpec {
    pub package_path: AnchoredSystemPathBuf,
    pub inputs: Option<GlobSet>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("package hashing encountered an error: {0}")]
    HashingError(String),
    #[error("file hashing is not available: {0}")]
    Unavailable(String),
    #[error("package not found: {} {:?}", .0.package_path, .0.inputs)]
    UnknownPackage(HashSpec),
}

// Communication errors that all funnel to Unavailable

impl From<watch::error::RecvError> for Error {
    fn from(e: watch::error::RecvError) -> Self {
        Self::Unavailable(e.to_string())
    }
}

impl From<oneshot::error::RecvError> for Error {
    fn from(e: oneshot::error::RecvError) -> Self {
        Self::Unavailable(e.to_string())
    }
}

impl<T> From<mpsc::error::SendError<T>> for Error {
    fn from(e: mpsc::error::SendError<T>) -> Self {
        Self::Unavailable(e.to_string())
    }
}

impl HashWatcher {
    pub fn new(
        repo_root: AbsoluteSystemPathBuf,
        package_discovery: watch::Receiver<Option<DiscoveryData>>,
        file_events: OptionalWatch<broadcast::Receiver<Result<Event, NotifyError>>>,
        scm: SCM,
    ) -> Self {
        let (exit_tx, exit_rx) = oneshot::channel();
        let (query_tx, query_rx) = mpsc::channel(16);
        let subscriber = Subscriber::new(repo_root, package_discovery, scm, query_rx);
        let handle = tokio::spawn(subscriber.watch(exit_rx, file_events));
        Self {
            _exit_tx: exit_tx,
            _handle: handle,
            query_tx,
        }
    }

    // Note that this does not wait for any sort of ready signal. The watching
    // process won't respond until filewatching is ready, but there is no
    // guarantee that package data or file hashing will be done before
    // responding. Both package discovery and file hashing can fail depending on the
    // state of the filesystem, so clients will need to be robust to receiving
    // errors.
    pub async fn get_file_hashes(&self, hash_spec: HashSpec) -> Result<GitHashes, Error> {
        let (tx, rx) = oneshot::channel();
        self.query_tx.send(Query::GetHash(hash_spec, tx)).await?;
        rx.await?
    }
}

struct Subscriber {
    repo_root: AbsoluteSystemPathBuf,
    package_discovery: watch::Receiver<Option<DiscoveryData>>,
    query_rx: mpsc::Receiver<Query>,
    scm: SCM,
}

enum Query {
    GetHash(HashSpec, oneshot::Sender<Result<GitHashes, Error>>),
}

// Version is a type that exists to stamp an asynchronous hash computation with
// a version so that we can ignore completion of outdated hash computations.
#[derive(Clone, Default)]
struct Version(Arc<()>);

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Version {}

struct HashDebouncer {
    bump: sync::Notify,
    serial: Mutex<Option<usize>>,
    timeout: Duration,
}

const DEFAULT_DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(10);

impl Default for HashDebouncer {
    fn default() -> Self {
        Self::new(DEFAULT_DEBOUNCE_TIMEOUT)
    }
}

impl HashDebouncer {
    fn new(timeout: Duration) -> Self {
        let bump = sync::Notify::new();
        let serial = Mutex::new(Some(0));
        Self {
            bump,
            serial,
            timeout,
        }
    }

    fn bump(&self) -> bool {
        let mut serial = self.serial.lock().expect("lock is valid");
        match *serial {
            None => false,
            Some(previous) => {
                *serial = Some(previous + 1);
                self.bump.notify_one();
                true
            }
        }
    }

    async fn debounce(&self) {
        let mut serial = {
            self.serial
                .lock()
                .expect("lock is valid")
                .expect("only this thread sets the value to None")
        };
        let mut deadline = Instant::now() + self.timeout;
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            select! {
                _ = self.bump.notified() => {
                    debug!("debouncer notified");
                    // reset timeout
                    let current_serial = self.serial.lock().expect("lock is valid").expect("only this thread sets the value to None");
                    if current_serial == serial {
                        // we timed out between the serial update and the notification.
                        // ignore this notification, we've already bumped the timer
                        continue;
                    } else {
                        serial = current_serial;
                        deadline = Instant::now() + self.timeout;
                    }
                }
                _ = timeout => {
                    // check if serial is still valid. It's possible a bump came in before the timeout,
                    // but we haven't been notified yet.
                    let mut current_serial_opt = self.serial.lock().expect("lock is valid");
                    let current_serial = current_serial_opt.expect("only this thread sets the value to None");
                    if current_serial == serial {
                        // if the serial is what we last observed, and the timer expired, we timed out.
                        // we're done. Mark that we won't accept any more bumps and return
                        *current_serial_opt = None;
                        return;
                    } else {
                        serial = current_serial;
                        deadline = Instant::now() + self.timeout;
                    }
                }
            }
        }
    }
}

enum HashState {
    Hashes(GitHashes),
    Pending(
        Version,
        Arc<HashDebouncer>,
        Vec<oneshot::Sender<Result<GitHashes, Error>>>,
    ),
    Unavailable(String),
}
// We use a radix_trie to store hashes so that we can quickly match a file path
// to a package without having to iterate over the set of all packages. We
// expect file changes to be the highest volume of events that this service
// handles, so we want to ensure we're efficient in deciding if a given change
// is relevant or not.
//
// Our Trie keys off of a String because of the orphan rule. Keys are required
// to be TrieKey, but this crate doesn't own TrieKey or AnchoredSystemPathBuf.
// We *could* implement TrieKey in AnchoredSystemPathBuf and avoid the String
// conversion, if we decide we want to add the radix_trie dependency to
// turbopath.
struct FileHashes(Trie<String, HashMap<Option<GlobSet>, HashState>>);

impl FileHashes {
    fn new() -> Self {
        Self(Trie::new())
    }

    fn drop_matching<F>(&mut self, mut f: F, reason: &str)
    where
        F: FnMut(&AnchoredSystemPath) -> bool,
    {
        let mut previous = std::mem::take(&mut self.0);

        // radix_trie doesn't have an into_iter() implementation, so we have a slightly
        // inefficient method for removing matching values. Fortunately, we only
        // need to do this when the package layout changes. It's O(n) in the
        // number of packages, on top of the trie internals.
        let keys = previous.keys().map(|k| k.to_owned()).collect::<Vec<_>>();
        for key in keys {
            let previous_value = previous
                .remove(&key)
                .expect("this key was pulled from previous");
            let path_key =
                AnchoredSystemPath::new(&key).expect("keys are valid AnchoredSystemPaths");
            if !f(path_key) {
                // keep it, we didn't match the key.
                self.0.insert(key, previous_value);
            } else {
                for state in previous_value.into_values() {
                    if let HashState::Pending(_, _, txs) = state {
                        for tx in txs {
                            let _ = tx.send(Err(Error::Unavailable(reason.to_string())));
                        }
                    }
                }
            }
        }
    }

    fn get_package_path(&self, file_path: &AnchoredSystemPath) -> Option<&AnchoredSystemPath> {
        self.0
            .get_ancestor(file_path.as_str())
            .and_then(|subtrie| subtrie.key())
            .map(|package_path| {
                AnchoredSystemPath::new(package_path).expect("keys are valid AnchoredSystemPaths")
            })
    }

    fn drain(&mut self, reason: &str) {
        // funnel through drop_matching even though we could just swap with a new trie.
        // We want to ensure we respond to any pending queries.
        self.drop_matching(|_| true, reason);
    }

    fn contains_key(&self, key: &HashSpec) -> bool {
        self.0
            .get(key.package_path.as_str())
            .and_then(|states| states.get(&key.inputs))
            .is_some()
    }

    fn insert(&mut self, key: HashSpec, value: HashState) {
        if let Some(states) = self.0.get_mut(key.package_path.as_str()) {
            states.insert(key.inputs, value);
        } else {
            let mut states = HashMap::new();
            states.insert(key.inputs, value);
            self.0.insert(key.package_path.as_str().to_owned(), states);
        }
    }

    fn get_mut(&mut self, key: &HashSpec) -> Option<&mut HashState> {
        self.0
            .get_mut(key.package_path.as_str())
            .and_then(|states| states.get_mut(&key.inputs))
    }
}

struct HashUpdate {
    spec: HashSpec,
    version: Version,
    result: Result<GitHashes, SCMError>,
}

impl Subscriber {
    fn new(
        repo_root: AbsoluteSystemPathBuf,
        package_discovery: watch::Receiver<Option<DiscoveryData>>,
        scm: SCM,
        query_rx: mpsc::Receiver<Query>,
    ) -> Self {
        Self {
            repo_root,
            package_discovery,
            scm,
            query_rx,
        }
    }

    async fn watch(
        mut self,
        mut exit_rx: oneshot::Receiver<()>,
        mut file_events: OptionalWatch<broadcast::Receiver<Result<Event, NotifyError>>>,
    ) {
        debug!("starting file hash watcher");
        let mut file_events_recv = match file_events.get().await {
            Ok(r) => r.resubscribe(),
            Err(e) => {
                debug!("file hash watcher exited: {:?}", e);
                return;
            }
        };
        let (hash_update_tx, mut hash_update_rx) = mpsc::channel::<HashUpdate>(16);
        let mut hashes = FileHashes::new();

        let mut package_data = self.package_discovery.borrow().to_owned();
        self.handle_package_data_update(&package_data, &mut hashes, &hash_update_tx);
        // We've gotten the ready signal from filewatching, and *some* state from
        // package discovery, but there is no guarantee that package discovery
        // is ready. This means that initial queries may be returned with errors
        // until we've completed package discovery and then hashing.
        //
        // This is the main event loop for the hash watcher. It receives file events,
        // updates to the package discovery state, and queries for hashes. It does
        // not use filesystem cookies, as it is expected that the client will
        // synchronize itself first before issuing a series of queries, one per
        // task that in the task graph for a run, and we don't want to block on
        // the filesystem for each query. This is analogous to running without
        // the daemon, where we assume a static filesystem for the duration of
        // generating task hashes.
        loop {
            select! {
                biased;
                _ = &mut exit_rx => {
                    debug!("file hash watcher exited");
                    return;
                },
                _ = self.package_discovery.changed() => {
                    self.package_discovery.borrow().clone_into(&mut package_data);
                    self.handle_package_data_update(&package_data, &mut hashes, &hash_update_tx);
                },
                file_event = file_events_recv.recv() => {
                    match file_event {
                        Ok(Ok(event)) => {
                            self.handle_file_event(event, &mut hashes, &hash_update_tx);
                        },
                        Ok(Err(e)) => {
                            debug!("file watcher error: {:?}", e);
                            self.flush_and_rehash(&mut hashes, &hash_update_tx, &package_data, &format!("file watcher error: {e}"));
                        },
                        Err(broadcast::error::RecvError::Closed) => {
                            debug!("file watcher closed");
                            hashes.drain("file watcher closed");
                            return;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            debug!("file watcher lagged");
                            self.flush_and_rehash(&mut hashes, &hash_update_tx, &package_data, "file watcher lagged");
                        },
                    }
                },
                hash_update = hash_update_rx.recv() => {
                    if let Some(hash_update) = hash_update {
                        self.handle_hash_update(hash_update, &mut hashes);
                    } else {
                        // note that we only ever lend out hash_update_tx, so this should be impossible
                        unreachable!("hash update channel closed, but we have a live reference to it");
                    }
                },
                Some(query) = self.query_rx.recv() => {
                    self.handle_query(query, &mut hashes);
                }
            }
        }
    }

    fn flush_and_rehash(
        &self,
        hashes: &mut FileHashes,
        hash_update_tx: &mpsc::Sender<HashUpdate>,
        package_data: &Option<Result<DiscoveryResponse, String>>,
        reason: &str,
    ) {
        // We need to send errors to any RPCs that are pending, and having an empty set
        // of hashes will cause handle_package_data_update to consider all
        // packages as new and rehash them.
        hashes.drain(reason);
        self.handle_package_data_update(package_data, hashes, hash_update_tx);
    }

    // We currently only support a single query, getting hashes for a given
    // HashSpec.
    fn handle_query(&self, query: Query, hashes: &mut FileHashes) {
        match query {
            Query::GetHash(spec, tx) => {
                if let Some(state) = hashes.get_mut(&spec) {
                    match state {
                        HashState::Hashes(hashes) => {
                            tx.send(Ok(hashes.clone())).unwrap();
                        }
                        HashState::Pending(_, _, txs) => {
                            txs.push(tx);
                        }
                        HashState::Unavailable(e) => {
                            let _ = tx.send(Err(Error::HashingError(e.clone())));
                        }
                    }
                } else {
                    let _ = tx.send(Err(Error::UnknownPackage(spec)));
                }
            }
        }
    }

    fn handle_hash_update(&self, update: HashUpdate, hashes: &mut FileHashes) {
        let HashUpdate {
            spec,
            version,
            result,
        } = update;
        // If we have a pending hash computation, update the state. If we don't, ignore
        // this update
        if let Some(state) = hashes.get_mut(&spec) {
            // We need mutable access to 'state' to update it, as well as being able to
            // extract the pending state, so we need two separate if statements
            // to pull the value apart.
            if let HashState::Pending(existing_version, _, pending_queries) = state {
                if *existing_version == version {
                    match result {
                        Ok(hashes) => {
                            debug!("updating hash at {:?}", spec.package_path);
                            for pending_query in pending_queries.drain(..) {
                                // We don't care if the client has gone away
                                let _ = pending_query.send(Ok(hashes.clone()));
                            }
                            *state = HashState::Hashes(hashes);
                        }
                        Err(e) => {
                            let error = e.to_string();
                            for pending_query in pending_queries.drain(..) {
                                // We don't care if the client has gone away
                                let _ = pending_query.send(Err(Error::HashingError(error.clone())));
                            }
                            *state = HashState::Unavailable(error);
                        }
                    }
                }
            }
        }
    }

    fn queue_package_hash(
        &self,
        spec: &HashSpec,
        hash_update_tx: &mpsc::Sender<HashUpdate>,
    ) -> (Version, Arc<HashDebouncer>) {
        let version = Version::default();
        let version_copy = version.clone();
        let tx = hash_update_tx.clone();
        let spec = spec.clone();
        let repo_root = self.repo_root.clone();
        let scm = self.scm.clone();
        let debouncer = Arc::new(HashDebouncer::default());
        let debouncer_copy = debouncer.clone();
        tokio::task::spawn(async move {
            debouncer_copy.debounce().await;
            // Package hashing involves blocking IO calls, so run on a blocking thread.
            tokio::task::spawn_blocking(move || {
                let telemetry = None;
                let inputs = spec.inputs.as_ref().map(|globs| globs.as_inputs());
                let result = scm.get_package_file_hashes(
                    &repo_root,
                    &spec.package_path,
                    inputs.as_deref().unwrap_or_default(),
                    telemetry,
                );
                let _ = tx.blocking_send(HashUpdate {
                    spec,
                    version: version_copy,
                    result,
                });
            });
        });
        (version, debouncer)
    }

    fn handle_file_event(
        &self,
        event: Event,
        hashes: &mut FileHashes,
        hash_update_tx: &mpsc::Sender<HashUpdate>,
    ) {
        let mut changed_packages: HashSet<AnchoredSystemPathBuf> = HashSet::new();
        for path in event.paths {
            let path = AbsoluteSystemPathBuf::try_from(path).expect("event path is a valid path");
            let repo_relative_change_path = self
                .repo_root
                .anchor(&path)
                .expect("event path is in the repository");
            // If this change is not relevant to a package, ignore it
            trace!("file change at {:?}", repo_relative_change_path);
            if let Some(package_path) = hashes.get_package_path(&repo_relative_change_path) {
                // We have a file change in a package, and we haven't seen this package yet.
                // Queue it for rehashing.
                // TODO: further qualification. Which sets of inputs? Is this file .gitignored?
                // We are somewhat saved here by deferring to the SCM to do the hashing. A
                // change to a gitignored file will trigger a re-hash, but won't
                // actually affect what the hash is.
                trace!("package changed: {:?}", package_path);
                changed_packages.insert(package_path.to_owned());
            } else {
                trace!("Ignoring change to {repo_relative_change_path}");
            }
        }
        // TODO: handle different sets of inputs
        for package_path in changed_packages {
            let spec = HashSpec {
                package_path,
                inputs: None,
            };
            match hashes.get_mut(&spec) {
                // Technically this shouldn't happen, the package_paths are sourced from keys in
                // hashes.
                None => {
                    let (version, debouncer) = self.queue_package_hash(&spec, hash_update_tx);
                    hashes.insert(spec, HashState::Pending(version, debouncer, vec![]));
                }
                Some(entry) => {
                    if let HashState::Pending(_, debouncer, txs) = entry {
                        if !debouncer.bump() {
                            // we failed to bump the debouncer, the hash must already be in
                            // progress. Drop this calculation and start
                            // a new one
                            let (version, debouncer) =
                                self.queue_package_hash(&spec, hash_update_tx);
                            let mut swap_target = vec![];
                            std::mem::swap(txs, &mut swap_target);
                            *entry = HashState::Pending(version, debouncer, swap_target);
                        }
                    } else {
                        // it's not a pending hash calculation, overwrite the entry with a new
                        // pending calculation
                        let (version, debouncer) = self.queue_package_hash(&spec, hash_update_tx);
                        *entry = HashState::Pending(version, debouncer, vec![]);
                    }
                }
            }
        }
    }

    fn handle_package_data_update(
        &self,
        package_data: &Option<Result<DiscoveryResponse, String>>,
        hashes: &mut FileHashes,
        hash_update_tx: &mpsc::Sender<HashUpdate>,
    ) {
        debug!("handling package data {:?}", package_data);
        match package_data {
            Some(Ok(data)) => {
                let package_paths: HashSet<AnchoredSystemPathBuf> =
                    HashSet::from_iter(data.workspaces.iter().map(|ws| {
                        self.repo_root
                            .anchor(
                                ws.package_json
                                    .parent()
                                    .expect("package.json is in a directory"),
                            )
                            .expect("package is in the repository")
                    }));
                // We have new package data. Drop any packages we don't need anymore, add any
                // new ones
                hashes.drop_matching(
                    |package_path| !package_paths.contains(package_path),
                    "package was removed",
                );
                for package_path in package_paths {
                    let spec = HashSpec {
                        package_path,
                        inputs: None,
                    };
                    if !hashes.contains_key(&spec) {
                        let (version, debouncer) = self.queue_package_hash(&spec, hash_update_tx);
                        hashes.insert(spec, HashState::Pending(version, debouncer, vec![]));
                    }
                }
                tracing::debug!("received package discovery data: {:?}", data);
            }
            None | Some(Err(_)) => {
                // package data invalidated, flush everything
                hashes.drain("package discovery is unavailable");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        assert_matches::assert_matches,
        sync::Arc,
        time::{Duration, Instant},
    };

    use git2::Repository;
    use tempfile::{tempdir, TempDir};
    use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf, RelativeUnixPathBuf};
    use turborepo_scm::{package_deps::GitHashes, SCM};

    use crate::{
        cookies::CookieWriter,
        hash_watcher::{HashDebouncer, HashSpec, HashWatcher},
        package_watcher::PackageWatcher,
        FileSystemWatcher,
    };

    fn commit_all(repo: &Repository) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree_oid = index.write_tree().unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let previous_commit = repo.head().ok().map(|r| r.peel_to_commit().unwrap());
        repo.commit(
            Some("HEAD"),
            &repo.signature().unwrap(),
            &repo.signature().unwrap(),
            "Commit",
            &tree,
            previous_commit
                .as_ref()
                .as_ref()
                .map(std::slice::from_ref)
                .unwrap_or_default(),
        )
        .unwrap();
    }

    fn setup_fixture() -> (TempDir, Repository, AbsoluteSystemPathBuf) {
        let tmp = tempdir().unwrap();
        let repo_root = AbsoluteSystemPathBuf::try_from(tmp.path())
            .unwrap()
            .to_realpath()
            .unwrap();
        let repo = Repository::init(&repo_root).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        // Setup npm workspaces, .gitignore for dist/ and two packages, one with a
        // nested .gitignore
        //
        // <repo_root>
        // ├── .gitignore (ignore dist/)
        // ├── package.json
        // ├── package-lock.json
        // ├── packages
        // │   ├── foo
        // │   │   ├── .gitignore (ignore out/)
        // │   │   ├── package.json
        // │   │   ├── foo-file
        // │   │   ├── dist
        // │   │   └── out
        // |   |── bar
        // |   |   ├── package.json
        // │   │   ├── dist
        // │   │   └── bar-file
        repo_root
            .join_component(".gitignore")
            .create_with_contents("dist/\n")
            .unwrap();
        repo_root
            .join_component("package.json")
            .create_with_contents(r#"{"workspaces": ["packages/*"]}"#)
            .unwrap();
        repo_root
            .join_component("package-lock.json")
            .create_with_contents("{}")
            .unwrap();
        let packages = repo_root.join_component("packages");

        let foo_dir = packages.join_component("foo");
        foo_dir.join_component("out").create_dir_all().unwrap();
        foo_dir.join_component("dist").create_dir_all().unwrap();
        foo_dir
            .join_component(".gitignore")
            .create_with_contents("out/\n")
            .unwrap();
        foo_dir
            .join_component("package.json")
            .create_with_contents(r#"{"name": "foo"}"#)
            .unwrap();
        foo_dir
            .join_component("foo-file")
            .create_with_contents("foo file contents")
            .unwrap();

        let bar_dir = packages.join_component("bar");
        bar_dir.join_component("dist").create_dir_all().unwrap();
        bar_dir
            .join_component("package.json")
            .create_with_contents(r#"{"name": "bar"}"#)
            .unwrap();
        bar_dir
            .join_component("bar-file")
            .create_with_contents("bar file contents")
            .unwrap();
        commit_all(&repo);

        (tmp, repo, repo_root)
    }

    fn create_fixture_branch(repo: &Repository, repo_root: &AbsoluteSystemPath) {
        // create a branch that deletes bar-file and adds baz-file to the bar package
        let bar_dir = repo_root.join_components(&["packages", "bar"]);
        bar_dir.join_component("bar-file").remove().unwrap();
        bar_dir
            .join_component("baz-file")
            .create_with_contents("baz file contents")
            .unwrap();
        let current_commit = repo
            .head()
            .ok()
            .map(|r| r.peel_to_commit().unwrap())
            .unwrap();
        repo.branch("test-branch", &current_commit, false).unwrap();
        repo.set_head("refs/heads/test-branch").unwrap();
        commit_all(&repo);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_basic_file_changes() {
        let (_tmp, _repo, repo_root) = setup_fixture();

        let watcher = FileSystemWatcher::new_with_default_cookie_dir(&repo_root).unwrap();

        let recv = watcher.watch();
        let cookie_writer = CookieWriter::new(
            watcher.cookie_dir(),
            Duration::from_millis(100),
            recv.clone(),
        );

        let scm = SCM::new(&repo_root);
        assert!(!scm.is_manual());
        let package_watcher = PackageWatcher::new(repo_root.clone(), recv, cookie_writer).unwrap();
        let package_discovery = package_watcher.watch_discovery();
        let hash_watcher =
            HashWatcher::new(repo_root.clone(), package_discovery, watcher.watch(), scm);

        let foo_path = repo_root.join_components(&["packages", "foo"]);
        // We need to give filewatching time to do the initial scan,
        // but this should resolve in short order to the expected value.
        retry_get_hash(
            &hash_watcher,
            HashSpec {
                package_path: repo_root.anchor(&foo_path).unwrap(),
                inputs: None,
            },
            Duration::from_secs(2),
            make_expected(vec![
                ("foo-file", "9317666a2e7b729b740c706ab79724952c97bde4"),
                ("package.json", "395351bdd7167f351af3396d3225ebe97a7a4d13"),
                (".gitignore", "89f9ac04aac6c8ee66e158853e7d0439b3ec782d"),
            ]),
        )
        .await;

        // update foo-file
        let foo_file_path = repo_root.join_components(&["packages", "foo", "foo-file"]);
        foo_file_path
            .create_with_contents("new foo-file contents")
            .unwrap();
        retry_get_hash(
            &hash_watcher,
            HashSpec {
                package_path: repo_root.anchor(&foo_path).unwrap(),
                inputs: None,
            },
            Duration::from_secs(2),
            make_expected(vec![
                ("foo-file", "5f6796bbd23dcdc9d30d07a2d8a4817c34b7f1e7"),
                ("package.json", "395351bdd7167f351af3396d3225ebe97a7a4d13"),
                (".gitignore", "89f9ac04aac6c8ee66e158853e7d0439b3ec782d"),
            ]),
        )
        .await;

        // update files in dist/ and out/ and foo-file
        // verify we don't get hashes for the gitignored files
        repo_root
            .join_components(&["packages", "foo", "out", "some-file"])
            .create_with_contents("an ignored file")
            .unwrap();
        repo_root
            .join_components(&["packages", "foo", "dist", "some-other-file"])
            .create_with_contents("an ignored file")
            .unwrap();
        foo_file_path
            .create_with_contents("even more foo-file contents")
            .unwrap();
        retry_get_hash(
            &hash_watcher,
            HashSpec {
                package_path: repo_root.anchor(&foo_path).unwrap(),
                inputs: None,
            },
            Duration::from_secs(2),
            make_expected(vec![
                ("foo-file", "0cb73634538618658f092cd7a3a373c243513a6a"),
                ("package.json", "395351bdd7167f351af3396d3225ebe97a7a4d13"),
                (".gitignore", "89f9ac04aac6c8ee66e158853e7d0439b3ec782d"),
            ]),
        )
        .await;
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_switch_branch() {
        let (_tmp, repo, repo_root) = setup_fixture();

        let watcher = FileSystemWatcher::new_with_default_cookie_dir(&repo_root).unwrap();

        let recv = watcher.watch();
        let cookie_writer = CookieWriter::new(
            watcher.cookie_dir(),
            Duration::from_millis(100),
            recv.clone(),
        );

        let scm = SCM::new(&repo_root);
        assert!(!scm.is_manual());
        let package_watcher = PackageWatcher::new(repo_root.clone(), recv, cookie_writer).unwrap();
        let package_discovery = package_watcher.watch_discovery();
        let hash_watcher =
            HashWatcher::new(repo_root.clone(), package_discovery, watcher.watch(), scm);

        let bar_path = repo_root.join_components(&["packages", "bar"]);

        // We need to give filewatching time to do the initial scan,
        // but this should resolve in short order to the expected value.
        retry_get_hash(
            &hash_watcher,
            HashSpec {
                package_path: repo_root.anchor(&bar_path).unwrap(),
                inputs: None,
            },
            Duration::from_secs(2),
            make_expected(vec![
                ("bar-file", "b9bdb1e4875f7397b3f68c104bc249de0ecd3f8e"),
                ("package.json", "b39117e03f0dbe217b957f58a2ad78b993055088"),
            ]),
        )
        .await;

        create_fixture_branch(&repo, &repo_root);

        retry_get_hash(
            &hash_watcher,
            HashSpec {
                package_path: repo_root.anchor(&bar_path).unwrap(),
                inputs: None,
            },
            Duration::from_secs(2),
            make_expected(vec![
                ("baz-file", "a5395ccf1b8966f3ea805aff0851eac13acb3540"),
                ("package.json", "b39117e03f0dbe217b957f58a2ad78b993055088"),
            ]),
        )
        .await;
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_non_existent_package() {
        let (_tmp, _repo, repo_root) = setup_fixture();

        let watcher = FileSystemWatcher::new_with_default_cookie_dir(&repo_root).unwrap();

        let recv = watcher.watch();
        let cookie_writer = CookieWriter::new(
            watcher.cookie_dir(),
            Duration::from_millis(100),
            recv.clone(),
        );

        let scm = SCM::new(&repo_root);
        assert!(!scm.is_manual());
        let package_watcher = PackageWatcher::new(repo_root.clone(), recv, cookie_writer).unwrap();
        let package_discovery = package_watcher.watch_discovery();
        let hash_watcher =
            HashWatcher::new(repo_root.clone(), package_discovery, watcher.watch(), scm);

        let non_existent_path = repo_root.join_components(&["packages", "non-existent"]);
        let relative_non_existent_path = repo_root.anchor(&non_existent_path).unwrap();
        let result = hash_watcher
            .get_file_hashes(HashSpec {
                package_path: relative_non_existent_path.clone(),
                inputs: None,
            })
            .await;
        assert_matches!(result, Err(crate::hash_watcher::Error::UnknownPackage(unknown_spec)) if unknown_spec.package_path == relative_non_existent_path);
    }

    // we don't have a signal for when hashing is complete after having made a file
    // change set a long timeout, but retry several times to try to hit the
    // success case quickly
    async fn retry_get_hash(
        hash_watcher: &HashWatcher,
        spec: HashSpec,
        timeout: Duration,
        expected: GitHashes,
    ) {
        let deadline = Instant::now() + timeout;
        let mut error = None;
        let mut last_value = None;
        while Instant::now() < deadline {
            match hash_watcher.get_file_hashes(spec.clone()).await {
                Ok(hashes) => {
                    if hashes == expected {
                        return;
                    } else {
                        last_value = Some(hashes);
                    }
                }
                Err(e) => {
                    error = Some(e);
                }
            }
        }
        panic!(
            "failed to get expected hashes. Error {:?}, last hashes: {:?}",
            error, last_value
        );
    }

    fn make_expected(expected: Vec<(&str, &str)>) -> GitHashes {
        let mut map = GitHashes::new();
        for (path, hash) in expected {
            map.insert(RelativeUnixPathBuf::new(path).unwrap(), hash.to_string());
        }
        map
    }

    #[tokio::test]
    async fn test_debouncer() {
        let debouncer = Arc::new(HashDebouncer::new(Duration::from_millis(10)));
        let debouncer_copy = debouncer.clone();
        let handle = tokio::task::spawn(async move {
            debouncer_copy.debounce().await;
        });
        for _ in 0..10 {
            // assert that we can continue bumping it past the original timeout
            tokio::time::sleep(Duration::from_millis(2)).await;
            assert!(debouncer.bump());
        }
        let start = Instant::now();
        handle.await.unwrap();
        let end = Instant::now();
        // give some wiggle room to account for race conditions, but assert that we
        // didn't immediately complete after the last bump
        assert!(end - start > Duration::from_millis(5));
        // we shouldn't be able to bump it after it's run out it's timeout
        assert!(!debouncer.bump());
    }
}
