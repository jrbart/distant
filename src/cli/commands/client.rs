use crate::{
    cli::{
        client::{MsgReceiver, MsgSender},
        Cache, Client,
    },
    config::{ClientConfig, ClientLaunchConfig, NetworkConfig},
    paths::user::CACHE_FILE_PATH_STR,
    CliError, CliResult,
};
use anyhow::Context;
use clap::{Subcommand, ValueHint};
use dialoguer::{console::Term, theme::ColorfulTheme, Select};
use distant_core::{
    data::{ChangeKindSet, Environment},
    net::{IntoSplit, Request, Response, TypedAsyncRead, TypedAsyncWrite},
    ConnectionId, Destination, DistantManagerClient, DistantMsg, DistantRequestData,
    DistantResponseData, Extra, RemoteCommand, Watcher,
};
use log::*;
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

mod buf;
mod format;
mod link;
mod lsp;
mod shell;
mod stdin;

pub use format::Format;
use format::Formatter;
use link::RemoteProcessLink;
use lsp::Lsp;
use shell::Shell;

#[derive(Debug, Subcommand)]
pub enum ClientSubcommand {
    /// Performs some action on a remote machine
    Action {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        /// Specify a connection being managed
        #[clap(long)]
        connection: Option<ConnectionId>,

        #[clap(flatten)]
        network: NetworkConfig,

        /// Represents the maximum time (in seconds) to wait for a network request before timing out
        #[clap(short, long)]
        timeout: Option<f32>,

        #[clap(subcommand)]
        request: DistantRequestData,
    },

    /// Requests that active manager connects to the server at the specified destination
    Connect {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        #[clap(flatten)]
        network: NetworkConfig,

        #[clap(short, long, default_value_t, value_enum)]
        format: Format,

        destination: Box<Destination>,
    },

    /// Launches the server-portion of the binary on a remote machine
    Launch {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        #[clap(flatten)]
        config: ClientLaunchConfig,

        #[clap(flatten)]
        network: NetworkConfig,

        #[clap(short, long, default_value_t, value_enum)]
        format: Format,

        destination: Box<Destination>,
    },

    /// Specialized treatment of running a remote LSP process
    Lsp {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        /// Specify a connection being managed
        #[clap(long)]
        connection: Option<ConnectionId>,

        #[clap(flatten)]
        network: NetworkConfig,

        /// If provided, will run in persist mode, meaning that the process will not be killed if the
        /// client disconnects from the server
        #[clap(long)]
        persist: bool,

        /// If provided, will run LSP in a pty
        #[clap(long)]
        pty: bool,

        cmd: String,
    },

    /// Runs actions in a read-eval-print loop
    Repl {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        /// Specify a connection being managed
        #[clap(long)]
        connection: Option<ConnectionId>,

        #[clap(flatten)]
        network: NetworkConfig,

        /// Format used for input into and output from the repl
        #[clap(short, long, default_value_t, value_enum)]
        format: Format,

        /// Represents the maximum time (in seconds) to wait for a network request before timing out
        #[clap(short, long)]
        timeout: Option<f32>,
    },

    /// Select the active connection
    Select {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        /// Connection to use, otherwise will prompt to select
        connection: Option<ConnectionId>,

        #[clap(flatten)]
        network: NetworkConfig,
    },

    /// Specialized treatment of running a remote shell process
    Shell {
        /// Location to store cached data
        #[clap(
            long,
            value_hint = ValueHint::FilePath,
            value_parser,
            default_value = CACHE_FILE_PATH_STR.as_str()
        )]
        cache: PathBuf,

        /// Specify a connection being managed
        #[clap(long)]
        connection: Option<ConnectionId>,

        #[clap(flatten)]
        network: NetworkConfig,

        /// Environment variables to provide to the shell
        #[clap(long, default_value_t)]
        environment: Environment,

        /// If provided, will run in persist mode, meaning that the process will not be killed if the
        /// client disconnects from the server
        #[clap(long)]
        persist: bool,

        /// Optional command to run instead of $SHELL
        cmd: Option<String>,
    },
}

impl ClientSubcommand {
    pub fn run(self, config: ClientConfig) -> CliResult {
        let rt = tokio::runtime::Runtime::new().context("Failed to start up runtime")?;
        rt.block_on(Self::async_run(self, config))
    }

    fn cache_path(&self) -> &Path {
        match self {
            Self::Action { cache, .. } => cache.as_path(),
            Self::Connect { cache, .. } => cache.as_path(),
            Self::Launch { cache, .. } => cache.as_path(),
            Self::Lsp { cache, .. } => cache.as_path(),
            Self::Repl { cache, .. } => cache.as_path(),
            Self::Select { cache, .. } => cache.as_path(),
            Self::Shell { cache, .. } => cache.as_path(),
        }
    }

    async fn async_run(self, config: ClientConfig) -> CliResult {
        let mut cache = Cache::read_from_disk_or_default(self.cache_path().to_path_buf()).await?;

        match self {
            Self::Action {
                connection,
                network,
                request,
                timeout,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = Client::new(network)
                    .connect()
                    .await
                    .context("Failed to connect to manager")?;

                let connection_id =
                    use_or_lookup_connection_id(&mut cache, connection, &mut client).await?;

                debug!("Opening channel to connection {}", connection_id);
                let mut channel = client.open_channel(connection_id).await.with_context(|| {
                    format!("Failed to open channel to connection {connection_id}")
                })?;

                debug!(
                    "Timeout configured to be {}",
                    match timeout {
                        Some(secs) => format!("{}s", secs),
                        None => "none".to_string(),
                    }
                );

                let formatter = Formatter::shell();

                debug!("Sending request {:?}", request);
                match request {
                    DistantRequestData::ProcSpawn {
                        cmd,
                        environment,
                        current_dir,
                        persist,
                        pty,
                    } => {
                        debug!("Special request spawning {:?}", cmd);
                        let mut proc = RemoteCommand::new()
                            .environment(environment)
                            .current_dir(current_dir)
                            .persist(persist)
                            .pty(pty)
                            .spawn(channel, cmd.as_str())
                            .await
                            .with_context(|| format!("Failed to spawn {cmd}"))?;

                        // Now, map the remote process' stdin/stdout/stderr to our own process
                        let link = RemoteProcessLink::from_remote_pipes(
                            proc.stdin.take(),
                            proc.stdout.take().unwrap(),
                            proc.stderr.take().unwrap(),
                        );

                        let status = proc.wait().await.context("Failed to wait for process")?;

                        // Shut down our link
                        link.shutdown().await;

                        if !status.success {
                            if let Some(code) = status.code {
                                return Err(CliError::Exit(code as u8));
                            } else {
                                return Err(CliError::FAILURE);
                            }
                        }
                    }
                    DistantRequestData::Watch {
                        path,
                        recursive,
                        only,
                        except,
                    } => {
                        debug!("Special request creating watcher for {:?}", path);
                        let mut watcher = Watcher::watch(
                            channel,
                            path.as_path(),
                            recursive,
                            only.into_iter().collect::<ChangeKindSet>(),
                            except.into_iter().collect::<ChangeKindSet>(),
                        )
                        .await
                        .with_context(|| format!("Failed to watch {path:?}"))?;

                        // Continue to receive and process changes
                        while let Some(change) = watcher.next().await {
                            // TODO: Provide a cleaner way to print just a change
                            let res = Response::new(
                                "".to_string(),
                                DistantMsg::Single(DistantResponseData::Changed(change)),
                            );

                            formatter.print(res).context("Failed to print change")?;
                        }
                    }
                    request => {
                        let response = channel
                            .send_timeout(
                                DistantMsg::Single(request),
                                timeout
                                    .or(config.action.timeout)
                                    .map(Duration::from_secs_f32),
                            )
                            .await
                            .context("Failed to send request")?;

                        debug!("Got response {:?}", response);

                        // NOTE: We expect a single response, and if that is an error then
                        //       we want to pass that error up the stack
                        let id = response.id;
                        let origin_id = response.origin_id;
                        match response.payload {
                            DistantMsg::Single(DistantResponseData::Error(x)) => {
                                return Err(CliError::Error(anyhow::anyhow!(x)));
                            }
                            payload => formatter
                                .print(Response {
                                    id,
                                    origin_id,
                                    payload,
                                })
                                .context("Failed to print response")?,
                        }
                    }
                }
            }
            Self::Connect {
                network,
                format,
                destination,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = {
                    let client = match format {
                        Format::Shell => Client::new(network),
                        Format::Json => Client::new(network).using_msg_stdin_stdout(),
                    };
                    client
                        .connect()
                        .await
                        .context("Failed to connect to manager")?
                };

                // Trigger our manager to connect to the launched server
                debug!("Connecting to server at {}", destination);
                let id = client
                    .connect(*destination, Extra::new())
                    .await
                    .context("Failed to connect to server")?;

                // Mark the server's id as the new default
                debug!("Updating selected connection id in cache to {}", id);
                *cache.data.selected = id;
                cache.write_to_disk().await?;

                println!("{}", id);
            }
            Self::Launch {
                config: launcher_config,
                network,
                format,
                destination,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = {
                    let client = match format {
                        Format::Shell => Client::new(network),
                        Format::Json => Client::new(network).using_msg_stdin_stdout(),
                    };
                    client
                        .connect()
                        .await
                        .context("Failed to connect to manager")?
                };

                // Merge our launch configs, overwriting anything in the config file
                // with our cli arguments
                let mut extra = Extra::from(config.launch);
                extra.extend(Extra::from(launcher_config).into_map());

                // Grab the host we are connecting to for later use
                let host = destination.to_host_string();

                // Start the server using our manager
                debug!("Launching server at {} with {}", destination, extra);
                let mut new_destination = client
                    .launch(*destination, extra)
                    .await
                    .context("Failed to launch server")?;

                // Update the new destination with our previously-used host if the
                // new host is not globally-accessible
                if !new_destination.is_host_global() {
                    trace!(
                        "Updating host to {:?} from non-global {:?}",
                        host,
                        new_destination.to_host_string()
                    );
                    new_destination
                        .replace_host(host.as_str())
                        .context("Failed to replace host")?;
                } else {
                    trace!("Host {:?} is global", new_destination.to_host_string());
                }

                // Trigger our manager to connect to the launched server
                debug!("Connecting to server at {}", new_destination);
                let id = client
                    .connect(new_destination, Extra::new())
                    .await
                    .context("Failed to connect to server")?;

                // Mark the server's id as the new default
                debug!("Updating selected connection id in cache to {}", id);
                *cache.data.selected = id;
                cache.write_to_disk().await?;

                println!("{}", id);
            }
            Self::Lsp {
                connection,
                network,
                persist,
                pty,
                cmd,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = Client::new(network)
                    .connect()
                    .await
                    .context("Failed to connect to manager")?;

                let connection_id =
                    use_or_lookup_connection_id(&mut cache, connection, &mut client).await?;

                debug!("Opening channel to connection {}", connection_id);
                let channel = client.open_channel(connection_id).await.with_context(|| {
                    format!("Failed to open channel to connection {connection_id}")
                })?;

                debug!(
                    "Spawning LSP server (persist = {}, pty = {}): {}",
                    persist, pty, cmd
                );
                Lsp::new(channel).spawn(cmd, persist, pty).await?;
            }
            Self::Repl {
                connection,
                network,
                format,
                timeout,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = Client::new(network)
                    .using_msg_stdin_stdout()
                    .connect()
                    .await
                    .context("Failed to connect to manager")?;

                let connection_id =
                    use_or_lookup_connection_id(&mut cache, connection, &mut client).await?;

                debug!("Opening raw channel to connection {}", connection_id);
                let channel = client
                    .open_raw_channel(connection_id)
                    .await
                    .with_context(|| {
                        format!("Failed to open raw channel to connection {connection_id}")
                    })?;

                debug!(
                    "Timeout configured to be {}",
                    match timeout {
                        Some(secs) => format!("{}s", secs),
                        None => "none".to_string(),
                    }
                );

                // TODO: Support shell format?
                if !format.is_json() {
                    return Err(CliError::Error(anyhow::anyhow!(
                        "Only JSON format is supported"
                    )));
                }

                debug!("Starting repl using format {:?}", format);
                let (mut writer, mut reader) = channel.transport.into_split();
                let response_task = tokio::task::spawn(async move {
                    let tx = MsgSender::from_stdout();
                    while let Some(response) = reader.read().await? {
                        debug!("Received response {:?}", response);
                        tx.send_blocking(&response)?;
                    }
                    io::Result::Ok(())
                });

                let request_task = tokio::spawn(async move {
                    let mut rx = MsgReceiver::from_stdin()
                        .into_rx::<Request<DistantMsg<DistantRequestData>>>();
                    loop {
                        match rx.recv().await {
                            Some(Ok(request)) => {
                                debug!("Sending request {:?}", request);
                                writer.write(request).await?;
                            }
                            Some(Err(x)) => error!("{}", x),
                            None => {
                                debug!("Shutting down repl");
                                break;
                            }
                        }
                    }
                    io::Result::Ok(())
                });

                let (r1, r2) = tokio::join!(request_task, response_task);
                match r1 {
                    Err(x) => error!("{}", x),
                    Ok(Err(x)) => error!("{}", x),
                    _ => (),
                }
                match r2 {
                    Err(x) => error!("{}", x),
                    Ok(Err(x)) => error!("{}", x),
                    _ => (),
                }

                debug!("Shutting down repl");
            }
            Self::Select {
                connection,
                network,
                ..
            } => match connection {
                Some(id) => {
                    *cache.data.selected = id;
                    cache.write_to_disk().await?;
                }
                None => {
                    let network = network.merge(config.network);
                    debug!("Connecting to manager");
                    let mut client = Client::new(network)
                        .connect()
                        .await
                        .context("Failed to connect to manager")?;
                    let list = client
                        .list()
                        .await
                        .context("Failed to get a list of managed connections")?;

                    if list.is_empty() {
                        return Err(CliError::Error(anyhow::anyhow!(
                            "No connection available in manager"
                        )));
                    }

                    trace!("Building selection prompt of {} choices", list.len());
                    let selected = list
                        .iter()
                        .enumerate()
                        .find_map(|(i, (id, _))| {
                            if *cache.data.selected == *id {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();

                    let items: Vec<String> = list
                        .iter()
                        .map(|(_, destination)| {
                            format!(
                                "{}{}{}",
                                destination
                                    .scheme()
                                    .map(|x| format!(r"{}://", x))
                                    .unwrap_or_default(),
                                destination.to_host_string(),
                                destination
                                    .port()
                                    .map(|x| format!(":{}", x))
                                    .unwrap_or_default()
                            )
                        })
                        .collect();

                    trace!("Rendering prompt");
                    let selected = Select::with_theme(&ColorfulTheme::default())
                        .items(&items)
                        .default(selected)
                        .interact_on_opt(&Term::stderr())
                        .context("Failed to render prompt")?;

                    match selected {
                        Some(index) => {
                            trace!("Selected choice {}", index);
                            if let Some((id, _)) = list.iter().nth(index) {
                                debug!("Updating selected connection id in cache to {}", id);
                                *cache.data.selected = *id;
                                cache.write_to_disk().await?;
                            }
                        }
                        None => {
                            debug!("No change in selection of default connection id");
                        }
                    }
                }
            },
            Self::Shell {
                connection,
                network,
                environment,
                persist,
                cmd,
                ..
            } => {
                let network = network.merge(config.network);
                debug!("Connecting to manager");
                let mut client = Client::new(network)
                    .connect()
                    .await
                    .context("Failed to connect to manager")?;

                let connection_id =
                    use_or_lookup_connection_id(&mut cache, connection, &mut client).await?;

                debug!("Opening channel to connection {}", connection_id);
                let channel = client.open_channel(connection_id).await.with_context(|| {
                    format!("Failed to open channel to connection {connection_id}")
                })?;

                debug!(
                    "Spawning shell (environment = {:?}, persist = {}): {}",
                    environment,
                    persist,
                    cmd.as_deref().unwrap_or(r"$SHELL")
                );
                Shell::new(channel).spawn(cmd, environment, persist).await?;
            }
        }

        Ok(())
    }
}

async fn use_or_lookup_connection_id(
    cache: &mut Cache,
    connection: Option<ConnectionId>,
    client: &mut DistantManagerClient,
) -> anyhow::Result<ConnectionId> {
    match connection {
        Some(id) => {
            trace!("Using specified connection id: {}", id);
            Ok(id)
        }
        None => {
            trace!("Looking up connection id");
            let list = client
                .list()
                .await
                .context("Failed to retrieve list of available connections")?;

            if list.contains_key(&cache.data.selected) {
                trace!("Using cached connection id: {}", cache.data.selected);
                Ok(*cache.data.selected)
            } else if list.is_empty() {
                trace!("Cached connection id is invalid as there are no connections");
                anyhow::bail!("There are no connections being managed! You need to start one!");
            } else if list.len() > 1 {
                trace!("Cached connection id is invalid and there are multiple connections");
                anyhow::bail!(
                    "There are multiple connections being managed! You need to pick one!"
                );
            } else {
                trace!("Cached connection id is invalid");
                *cache.data.selected = *list.keys().next().unwrap();
                trace!(
                    "Detected singular connection id, so updating cache: {}",
                    cache.data.selected
                );
                cache.write_to_disk().await?;
                Ok(*cache.data.selected)
            }
        }
    }
}