use std::sync::{Arc, Mutex};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Instant, Duration};

use anyhow::{Result, anyhow};
use log::*;
use gethostname::gethostname;
use tokio::sync::mpsc::{Sender, Receiver};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::Config;
use crate::registry::{Registry, PkgRef};
use crate::containers::{DockerContainers, ContainerInfo, ContainerEvent};
use crate::open_monitor::{OpenMonitor, OpenEvent};
use crate::sbom::Sbom;

const BASEOS_ID_PATH: &str = "/var/lib/edgebit/baseos-id";

const OPEN_EVENT_LAG: Duration = Duration::from_secs(5);

pub enum Event {
    ContainerStarted(String, ContainerInfo),
    ContainerStopped(String, ContainerInfo),
    PackageInUse(String, Vec<PkgRef>),
}

struct OpenEventQueueItem {
    timestamp: Instant,
    evt: OpenEvent,
}

struct Inner {
    config: Arc<Config>,
    containers: DockerContainers,
    open_monitor: OpenMonitor,
    host_workload: Mutex<HostWorkload>,
    container_workloads: Mutex<HashMap<String, ContainerWorkload>>,
    events: Sender<Event>,
    open_event_q: Mutex<VecDeque<OpenEventQueueItem>>,
}

pub struct WorkloadManager {
    inner: Arc<Inner>,
    run_task: JoinHandle<()>,
    open_event_task: JoinHandle<()>,
}

impl WorkloadManager {
    pub async fn start(host_sbom: Sbom, config: Arc<Config>, events: Sender<Event>) -> Result<Self> {
        let mut host_includes = PathSet::new(PathBuf::from("/"))?;
        for path in config.host_includes() {
            // ignore the error as it's most likely from a missing path (which is ok)
            _ = host_includes.add(&PathBuf::from(path));
        }

        // load() will touch a lot of files so do that before starting to
        // monitor for open files
        let host_workload = HostWorkload::load(host_sbom, host_includes)?;

        let (open_tx, open_rx) = tokio::sync::mpsc::channel::<OpenEvent>(1000);
        let open_monitor = OpenMonitor::start(open_tx)?;

        host_workload.start_monitoring(&open_monitor);

        let (cont_tx, cont_rx) = tokio::sync::mpsc::channel::<ContainerEvent>(10);
        let containers = DockerContainers::track(config.docker_host(), cont_tx);

        let inner = Arc::new(Inner{
            config,
            containers,
            open_monitor,
            host_workload: Mutex::new(host_workload),
            container_workloads: Mutex::new(HashMap::new()),
            events,
            open_event_q: Mutex::new(VecDeque::new()),
        });

        let run_task = tokio::task::spawn(
            run(inner.clone(), open_rx, cont_rx)
        );

        let open_event_task = tokio::task::spawn(
            service_open_event_queue(inner.clone())
        );

        Ok(Self{
            inner,
            run_task,
            open_event_task,
        })
    }

    // Somewhat gross but easiest for now
    pub fn with_host_workload<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&HostWorkload) -> T
    {
        let w = self.inner.host_workload.lock().unwrap();
        f(&*w)
    }
}

async fn run(inner: Arc<Inner>, mut open_rx: Receiver<OpenEvent>, mut cont_rx: Receiver<ContainerEvent>) {
    loop {
        tokio::select!{
            evt = open_rx.recv() => {
                match evt {
                    Some(evt) => queue_open_event(&inner, evt),
                    None => break,
                }
            },
            evt = cont_rx.recv() => {
                match evt {
                    Some(evt) => handle_container_event(&inner, evt).await,
                    None => break,
                }
            }
        }
    }
}

fn queue_open_event(inner: &Inner, evt: OpenEvent) {
    inner.open_event_q.lock()
        .unwrap()
        .push_back(OpenEventQueueItem{
                timestamp: Instant::now(),
                evt,
            });
}

fn pop_open_event(inner: &Inner, cutoff: Instant) -> Option<OpenEventQueueItem> {
    let mut q = inner.open_event_q.lock().unwrap();
    if q.front()?.timestamp > cutoff {
        None
    } else {
        q.pop_front()
    }
}

async fn service_open_event_queue(inner: Arc<Inner>) {
    loop {
        let cutoff = Instant::now()
            .checked_sub(OPEN_EVENT_LAG)
            .unwrap();

        while let Some(item) = pop_open_event(&inner, cutoff) {
            handle_open_event(&inner, item.evt).await;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn handle_open_event(inner: &Inner, evt: OpenEvent) {
    match evt.filename.into_string() {
        Ok(filename) => {
            let cgroup = evt.cgroup_name.unwrap_or(String::new());
            debug!("[{cgroup}]: {filename}");

            let filepath = PathBuf::from(&filename);
            let filenames = vec![filename];

            let in_use = match inner.containers.id_from_cgroup(&cgroup) {
                Some(id) => {
                    debug!("Container match: {id}");

                    let container_workloads = inner.container_workloads.lock().unwrap();
                    if let Some(ref workload) = container_workloads.get(&id) {
                        if !workload.is_path_included(&filepath) {
                            return;
                        }

                        let pkg = PkgRef{
                            id: String::new(),
                            filenames,
                        };

                        Event::PackageInUse(id, vec![pkg])
                    } else {
                        error!("Container workload missing for id={id}");
                        return;
                    }
                },
                None => {
                    let host_workload = inner.host_workload.lock().unwrap();

                    if !host_workload.is_path_included(&filepath) {
                        return;
                    }

                    let pkgs = host_workload.pkgs.get_packages(filenames);
                    if pkgs.is_empty() {
                        return;
                    }

                    Event::PackageInUse(host_workload.id.clone(), pkgs)
                }
            };

            if let Err(err) = inner.events.send(in_use).await {
                error!("failed to send events on a channel: {err}");
            }
        },

        Err(name) => {
            error!("Non UTF-8 filename opened: {}", name.to_string_lossy());
        }
    }
}

async fn handle_container_event(inner: &Inner, evt: ContainerEvent) {
    match evt {
        ContainerEvent::Started(id, info) => {
            match &info.rootfs {
                Some(rootfs) => {
                    match ContainerWorkload::new(PathBuf::from(rootfs), &inner.config.container_includes()) {
                        Ok(workload) => {
                            workload.start_monitoring(&inner.open_monitor);

                            inner.container_workloads
                                .lock()
                                .unwrap()
                                .insert(id.clone(), workload);
                        },
                        Err(err) => error!("Failed to create a container workload: {err}"),
                    }
                },
                None => error!("Container {id} started but rootfs missing"),
            }

            if let Err(err) = inner.events.send(Event::ContainerStarted(id, info)).await {
                error!("failed to send events on a channel: {err}");
            }
        },
        ContainerEvent::Stopped(id, info) => {
            match &info.rootfs {
                Some(_) => {
                    let mut container_workloads = inner.container_workloads.lock().unwrap();
                    if let Some(workload) = container_workloads.remove(&id) {
                        workload.stop_monitoring(&inner.open_monitor);
                    }
                },
                None => error!("Container {id} stopped but rootfs missing"),
            }

            if let Err(err) = inner.events.send(Event::ContainerStopped(id, info)).await {
                error!("failed to send events on a channel: {err}");
            }
        }
    };
}

pub struct HostWorkload {
    pub id: String,
    pub group: Vec<String>,
    pub host: String,
    pub os_pretty_name: String,
    pub image_id: String,
    pkgs: Registry,
    includes: PathSet,
}

impl HostWorkload {
    fn load(sbom: Sbom, includes: PathSet) -> Result<Self> {
        let id = load_baseos_id();

        let host = gethostname()
            .to_string_lossy()
            .into_owned();

        let os_pretty_name = match rs_release::get_os_release() {
            Ok(mut os_release) => {
                os_release.remove("PRETTY_NAME")
                    .or_else(|| os_release.remove("NAME"))
                    .unwrap_or("Linux".to_string())
            },
            Err(err) => {
                error!("Failed to retrieve os-release: {err}");
                String::new()
            }
        };

        Ok(Self{
            id,
            group: Vec::new(),
            host,
            os_pretty_name,
            image_id: sbom.id(),
            pkgs: Registry::from_sbom(&sbom)?,
            includes,
        })
    }

    fn start_monitoring(&self, monitor: &OpenMonitor) {
        for path in self.includes.full_paths() {
            if let Err(err) = monitor.add_path(&path) {
                error!("Failed to start monitoring {} for container: {err}", path.display());
            }
        }
    }

    fn stop_monitoring(&self, monitor: &OpenMonitor) {
        for path in self.includes.full_paths() {
            _ = monitor.remove_path(&path);
        }
    }

    fn is_path_included(&self, path: &Path) -> bool {
        self.includes.contains(path)
    }
}

fn load_baseos_id() -> String {
    if let Ok(id) = std::fs::read_to_string(BASEOS_ID_PATH) {
        return id;
    }

    let id = uuid_string();

    if let Err(err) = std::fs::write(BASEOS_ID_PATH, &id) {
        error!("Failed to save BaseOS workload ID to {BASEOS_ID_PATH}: {err}");
    }

    id
}

fn uuid_string() -> String {
    let mut buf = Uuid::encode_buffer();
    Uuid::new_v4()
        .as_hyphenated()
        .encode_lower(&mut buf)
        .to_string()
}
struct ContainerWorkload {
    includes: PathSet,
}

impl ContainerWorkload {
    fn new(rootfs: PathBuf, includes: &[String]) -> Result<Self> {
        let mut includes_set = PathSet::new(rootfs)?;
        for path in includes {
            // ignore the error as it's most likely from a missing path (which is ok)
            _ = includes_set.add(&PathBuf::from(path));
        }

        Ok(Self{
            includes: includes_set,
        })
    }

    fn is_path_included(&self, path: &Path) -> bool {
        self.includes.contains(path)
    }

    fn start_monitoring(&self, monitor: &OpenMonitor) {
        for path in self.includes.full_paths() {
            if let Err(err) = monitor.add_path(&path) {
                error!("Failed to start monitoring {} for container: {err}", path.display());
            }
        }
    }

    fn stop_monitoring(&self, monitor: &OpenMonitor) {
        for path in self.includes.full_paths() {
            _ = monitor.remove_path(&path);
        }
    }
}

struct PathSet {
    base: PathBuf,
    members: HashMap<PathBuf, ()>,
}

impl PathSet {
    fn new(base: PathBuf) -> Result<Self> {
        Ok(Self {
            base: base.canonicalize()?,
            members: HashMap::new(),
        })
    }

    // adss the path, resolving the symlinks first
    fn add(&mut self, rel_path: &Path) -> Result<()> {
        // The rel_path is given relative to the base (chroot dir)
        // but should actually be absolute
        if !rel_path.is_absolute() {
            return Err(anyhow!("{} is not an absolute path", rel_path.to_string_lossy()));
        }

        let mut full_path = self.base.clone();
        // strip the leading "/"" so that path joining works
        full_path.push(rel_path.strip_prefix("/").unwrap());
        full_path = full_path.canonicalize()?;

        // Now that the symlinks have been removed, turn it back to relative to base
        let rel_path = full_path.strip_prefix(&self.base)?;

        let key = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else {
            PathBuf::from("/").join(rel_path)
        };

        self.members.insert(key, ());

        Ok(())
    }

    fn contains(&self, rel_path: &Path) -> bool {
        self.members
            .keys()
            .any(|f| rel_path.starts_with(f))
    }

    fn full_paths<'a>(&'a self) -> impl Iterator<Item=PathBuf> + 'a {
        self.members.keys()
            .map(|p| {
                let mut path = self.base.clone();
                path.push(p.strip_prefix("/").unwrap());
                path
            })
    }
}
