use crate::{input::Input, JOB_NAME_PROPERTY};
use anyhow::{ensure, Result};
use camino::Utf8PathBuf;
use dialoguer::console::style;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use std::ffi::OsStr;
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub(crate) enum Step {
    Comment(String),
    CreateDataset {
        dataset: String,
        // `None` here means to inherit the mountpoint property
        mountpoint: Option<Utf8PathBuf>,
        create_parents: bool,
        chown: String,
    },
    DestroyDataset {
        dataset: String,
    },
    DownloadArtefacts(Vec<DownloadArtefact>),
    InheritDatasetMountpoint {
        dataset: String,
    },
    RunScript {
        script: Utf8PathBuf,
        workdir: Utf8PathBuf,
    },
    SaveWorkAsInput {
        work_dataset: String,
        new_dataset: String,
        job_name: String,
        input: Input,
    },
    SetDatasetMountpoint {
        dataset: String,
        mountpoint: Utf8PathBuf,
    },
    SetDatasetReadOnly {
        dataset: String,
    },
}

impl Step {
    fn commands(&self) -> Vec<Command> {
        macro_rules! zfs {
            ($($arg:expr),*) => {{
                let mut command = Command::new("pfexec");
                command.arg("zfs");
                $(
                    command.arg($arg);
                )*
                command
            }};
        }

        match self {
            Step::Comment(_) | Step::DownloadArtefacts(_) => Vec::new(),
            Step::CreateDataset {
                dataset,
                mountpoint,
                create_parents,
                chown,
            } => {
                let mut create_cmd = zfs!["create"];
                if *create_parents {
                    create_cmd.arg("-p");
                }
                if let Some(mountpoint) = mountpoint {
                    create_cmd
                        .arg("-o")
                        .arg(format!("mountpoint={}", mountpoint));
                }
                create_cmd.arg(dataset);
                let mut commands = vec![create_cmd];

                if let Some(mountpoint) = mountpoint {
                    let mut chown_cmd = Command::new("pfexec");
                    chown_cmd.arg("chown").arg(chown).arg(mountpoint);
                    commands.push(chown_cmd);
                }

                commands
            }
            Step::DestroyDataset { dataset } => vec![zfs!["destroy", dataset]],
            Step::InheritDatasetMountpoint { dataset } => {
                vec![zfs!["inherit", "mountpoint", dataset]]
            }
            Step::RunScript { script, workdir } => {
                let mut command = Command::new("/bin/bash");
                command.arg(script);
                command.env_clear();
                command.current_dir(workdir);
                command.stdin(Stdio::null());
                vec![command]
            }
            Step::SaveWorkAsInput {
                work_dataset,
                new_dataset,
                job_name,
                ..
            } => {
                let snapshot = format!("{}@snapshot", work_dataset);
                vec![
                    zfs!["snapshot", &snapshot],
                    zfs![
                        "clone",
                        "-p",
                        "-o",
                        "readonly=on",
                        "-o",
                        format!("{}={}", JOB_NAME_PROPERTY, job_name),
                        &snapshot,
                        &new_dataset
                    ],
                    zfs!["promote", &new_dataset],
                ]
            }
            Step::SetDatasetMountpoint {
                dataset,
                mountpoint,
            } => vec![zfs!["set", format!("mountpoint={}", mountpoint), dataset]],
            Step::SetDatasetReadOnly { dataset } => vec![zfs!["set", "readonly=on", dataset]],
        }
    }

    pub(crate) fn commands_for_approval(&self) -> Vec<String> {
        match self {
            Step::Comment(comment) => {
                vec![style(format!("### {}", comment))
                    .cyan()
                    .italic()
                    .to_string()]
            }
            _ => self
                .commands()
                .into_iter()
                .map(|command| command_to_str(&command))
                .collect(),
        }
    }

    pub(crate) async fn run(&self, client: &Client) -> Result<()> {
        if let Step::DownloadArtefacts(artefacts) = self {
            eprintln!(
                "{} downloading {} artefacts to /input",
                style("==>").blue(),
                artefacts.len()
            );
            let progress = MultiProgress::new();
            let progress_meta = progress.add(
                ProgressBar::new_spinner().with_style(
                    ProgressStyle::with_template(
                        "[{elapsed_precise}] {wide_msg} total: {total_bytes} ({bytes_per_sec})",
                    )
                    .unwrap(),
                ),
            );
            let style = ProgressStyle::with_template(
                "{bar} {wide_msg} {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap();
            stream::iter(artefacts)
                .map(|artefact| artefact.download(client, &progress, &progress_meta, style.clone()))
                .buffer_unordered(4)
                .try_collect::<()>()
                .await?;
        } else {
            for mut command in self.commands() {
                let command_str = command_to_str(&command);
                eprintln!("{} {}", style("==>").blue(), command_str);
                let status = command.status()?;
                ensure!(status.success(), "`{}` failed with {}", command_str, status);
            }
        }

        if let Step::SaveWorkAsInput { input, .. } = self {
            eprintln!(
                "{} saved /work as input {}",
                style("==>").blue(),
                style(input).green()
            );
        }

        Ok(())
    }
}

fn command_to_str(command: &Command) -> String {
    shell_words::join(
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(OsStr::to_string_lossy),
    )
}

#[derive(Debug)]
pub(crate) struct DownloadArtefact {
    pub(crate) path: Utf8PathBuf,
    pub(crate) url: String,
}

impl DownloadArtefact {
    async fn download(
        &self,
        client: &Client,
        progress: &MultiProgress,
        progress_meta: &ProgressBar,
        style: ProgressStyle,
    ) -> Result<()> {
        let parent = self
            .path
            .parent()
            .expect("download path must have parent directory");
        std::fs::create_dir_all(parent)?;
        let (file, temp) = NamedTempFile::new_in(parent)?.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut response = client.get(&self.url).send().await?.error_for_status()?;
        let pbar = progress.insert_from_back(
            1,
            ProgressBar::new(response.content_length().unwrap_or_default())
                .with_style(style.clone())
                .with_message(self.path.to_string()),
        );
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            pbar.inc(chunk.len().try_into().unwrap());
            progress_meta.inc(chunk.len().try_into().unwrap());
        }
        file.flush().await?;
        temp.persist(&self.path)?;
        pbar.finish();
        Ok(())
    }
}
