use crate::data::{DistantResponseData, Environment, ProcessId, PtySize};
use distant_net::Reply;
use std::{collections::HashMap, io, ops::Deref, path::PathBuf};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

mod instance;
pub use instance::*;

/// Holds information related to spawned processes on the server
pub struct ProcessState {
    channel: ProcessChannel,
    task: JoinHandle<()>,
}

impl Drop for ProcessState {
    /// Aborts the task that handles process operations and management
    fn drop(&mut self) {
        self.abort();
    }
}

impl ProcessState {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(1);
        let task = tokio::spawn(process_task(tx.clone(), rx));

        Self {
            channel: ProcessChannel { tx },
            task,
        }
    }

    pub fn clone_channel(&self) -> ProcessChannel {
        self.channel.clone()
    }

    /// Aborts the process task
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Deref for ProcessState {
    type Target = ProcessChannel;

    fn deref(&self) -> &Self::Target {
        &self.channel
    }
}

#[derive(Clone)]
pub struct ProcessChannel {
    tx: mpsc::Sender<InnerProcessMsg>,
}

impl Default for ProcessChannel {
    /// Creates a new channel that is closed by default
    fn default() -> Self {
        let (tx, _) = mpsc::channel(1);
        Self { tx }
    }
}

impl ProcessChannel {
    /// Spawns a new process, returning the id associated with it
    pub async fn spawn(
        &self,
        cmd: String,
        environment: Environment,
        current_dir: Option<PathBuf>,
        persist: bool,
        pty: Option<PtySize>,
        reply: Box<dyn Reply<Data = DistantResponseData>>,
    ) -> io::Result<ProcessId> {
        let (cb, rx) = oneshot::channel();
        self.tx
            .send(InnerProcessMsg::Spawn {
                cmd,
                environment,
                current_dir,
                persist,
                pty,
                reply,
                cb,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Internal process task closed"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Response to spawn dropped"))?
    }

    /// Resizes the pty of a running process
    pub async fn resize_pty(&self, id: ProcessId, size: PtySize) -> io::Result<()> {
        let (cb, rx) = oneshot::channel();
        self.tx
            .send(InnerProcessMsg::Resize { id, size, cb })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Internal process task closed"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Response to resize dropped"))?
    }

    /// Send stdin to a running process
    pub async fn send_stdin(&self, id: ProcessId, data: Vec<u8>) -> io::Result<()> {
        let (cb, rx) = oneshot::channel();
        self.tx
            .send(InnerProcessMsg::Stdin { id, data, cb })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Internal process task closed"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Response to stdin dropped"))?
    }

    /// Kills a running process
    pub async fn kill(&self, id: ProcessId) -> io::Result<()> {
        let (cb, rx) = oneshot::channel();
        self.tx
            .send(InnerProcessMsg::Kill { id, cb })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Internal process task closed"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Response to kill dropped"))?
    }
}

/// Internal message to pass to our task below to perform some action
enum InnerProcessMsg {
    Spawn {
        cmd: String,
        environment: Environment,
        current_dir: Option<PathBuf>,
        persist: bool,
        pty: Option<PtySize>,
        reply: Box<dyn Reply<Data = DistantResponseData>>,
        cb: oneshot::Sender<io::Result<ProcessId>>,
    },
    Resize {
        id: ProcessId,
        size: PtySize,
        cb: oneshot::Sender<io::Result<()>>,
    },
    Stdin {
        id: ProcessId,
        data: Vec<u8>,
        cb: oneshot::Sender<io::Result<()>>,
    },
    Kill {
        id: ProcessId,
        cb: oneshot::Sender<io::Result<()>>,
    },
    InternalRemove {
        id: ProcessId,
    },
}

async fn process_task(tx: mpsc::Sender<InnerProcessMsg>, mut rx: mpsc::Receiver<InnerProcessMsg>) {
    let mut processes: HashMap<ProcessId, ProcessInstance> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            InnerProcessMsg::Spawn {
                cmd,
                environment,
                current_dir,
                persist,
                pty,
                reply,
                cb,
            } => {
                let _ = cb.send(
                    match ProcessInstance::spawn(cmd, environment, current_dir, persist, pty, reply)
                    {
                        Ok(mut process) => {
                            let id = process.id;

                            // Attach a callback for when the process is finished where
                            // we will remove it from our above list
                            let tx = tx.clone();
                            process.on_done(move |_| async move {
                                let _ = tx.send(InnerProcessMsg::InternalRemove { id }).await;
                            });

                            processes.insert(id, process);
                            Ok(id)
                        }
                        Err(x) => Err(x),
                    },
                );
            }
            InnerProcessMsg::Resize { id, size, cb } => {
                let _ = cb.send(match processes.get(&id) {
                    Some(process) => process.pty.resize_pty(size),
                    None => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("No process found with id {}", id),
                    )),
                });
            }
            InnerProcessMsg::Stdin { id, data, cb } => {
                let _ = cb.send(match processes.get_mut(&id) {
                    Some(process) => match process.stdin.as_mut() {
                        Some(stdin) => stdin.send(&data).await,
                        None => Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("Process {} stdin is closed", id),
                        )),
                    },
                    None => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("No process found with id {}", id),
                    )),
                });
            }
            InnerProcessMsg::Kill { id, cb } => {
                let _ = cb.send(match processes.get_mut(&id) {
                    Some(process) => process.killer.kill().await,
                    None => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("No process found with id {}", id),
                    )),
                });
            }
            InnerProcessMsg::InternalRemove { id } => {
                processes.remove(&id);
            }
        }
    }
}