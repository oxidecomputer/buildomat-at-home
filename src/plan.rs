use crate::input::Input;
use crate::step::{DownloadArtefact, Step};
use crate::{JOB_NAME_PROPERTY, OUR_DATASET, POOL};
use anyhow::{ensure, Context, Result};
use camino::Utf8Path;
use comrak::{nodes::NodeValue, Arena, ComrakOptions};
use dialoguer::Confirm;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use ulid::Ulid;

#[derive(Debug)]
pub(crate) struct Plan(pub(crate) Vec<Step>);

impl Plan {
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn build(
        client: &Client,
        script: &Utf8Path,
        inputs: &[Input],
    ) -> Result<Plan> {
        let frontmatter = FrontMatter::from_job(script)?;
        // Jobs are found in `.github/buildomat/jobs/whatever.sh`; remove that to
        // get the working directory.
        let workdir = script
            .ancestors()
            .nth(4)
            .context("failed to determine work dir")?
            .to_owned();
        ensure!(
            Some(
                workdir
                    .join(".github")
                    .join("buildomat")
                    .join("jobs")
                    .as_path()
            ) == script.parent(),
            "script path not within `.github/buildomat/jobs`"
        );
        let chown = ["-u", "-g"]
            .into_iter()
            .map(|arg| {
                let output = Command::new("id").arg(arg).output()?;
                ensure!(output.status.success(), "`id {}` failed", arg);
                Ok(std::str::from_utf8(&output.stdout)?.trim().to_owned())
            })
            .collect::<Result<Vec<_>>>()?
            .join(":");

        // Phase 1: Set up rpool/{buildomat-at-home,input,work}
        let mut plan = Vec::new();
        if !dataset_exists(OUR_DATASET)? {
            plan.push(Step::CreateDataset {
                dataset: OUR_DATASET.into(),
                mountpoint: None,
                create_parents: false,
                chown: chown.clone(),
            });
        }
        let input = format!("{}/input", POOL);
        if !dataset_exists(&input)? {
            plan.push(Step::CreateDataset {
                dataset: input,
                mountpoint: Some("/input".into()),
                create_parents: false,
                chown: chown.clone(),
            });
        }
        let work = format!("{}/work", POOL);
        if dataset_exists(&work)? {
            plan.push(Step::DestroyDataset {
                dataset: work.clone(),
            });
        }
        plan.push(Step::CreateDataset {
            dataset: work.clone(),
            mountpoint: Some("/work".into()),
            create_parents: false,
            chown: chown.clone(),
        });

        // Phase 2: Remove any of our mounts out from /input
        let output = Command::new("zfs")
            .args(["list", "-H", "-o", "name,mountpoint", "-r", OUR_DATASET])
            .stderr(Stdio::inherit())
            .output()?;
        ensure!(
            output.status.success(),
            "`zfs list -r {}` failed",
            OUR_DATASET
        );
        let output = String::from_utf8(output.stdout)?;
        for line in output.lines() {
            if let Some((dataset, mountpoint)) = line.split_once('\t') {
                if mountpoint.starts_with("/input") {
                    plan.push(Step::InheritDatasetMountpoint {
                        dataset: dataset.into(),
                    });
                }
            }
        }

        // Phase 3: Set up input mounts and download artifacts
        // FIXME: error if there's not enough inputs
        // FIXME: delete any existing snapshots that are not set readonly, as they may have failed from previous runs
        let mut pre_download_phase = Vec::new();
        let mut post_download_phase = Vec::new();
        let mut downloads = Vec::new();
        for input in inputs {
            let dataset = format!("{}/{}", OUR_DATASET, input);
            let mut check = None;
            let job_name = match input {
                Input::LocalBuild { .. } => {
                    let output = Command::new("zfs")
                        .args(["get", "-H", "-o", "value", JOB_NAME_PROPERTY, &dataset])
                        .output()?;
                    ensure!(output.status.success(), "input {} not found", input);
                    String::from_utf8(output.stdout)?
                }
                Input::GitHubRun {
                    owner,
                    repo,
                    run_id,
                } => {
                    let url = format!(
                        "https://api.github.com/repos/{}/{}/check-runs/{}",
                        owner, repo, run_id
                    );
                    let the_check: GitHubCheck = client.get(url).send().await?.json().await?;
                    let name = the_check.name.clone();
                    check = Some(the_check);
                    name
                }
            };
            let Some((k, _)) = frontmatter
            .dependencies
            .iter()
            .find(|(_, v)| v.job == job_name)
        else {
            continue;
        };
            let mountpoint = Utf8Path::new("/input").join(k);
            if let Some(check) = check {
                if dataset_exists(&dataset)? {
                    pre_download_phase.push(Step::SetDatasetMountpoint {
                        dataset: dataset.clone(),
                        mountpoint,
                    });
                } else {
                    for (path, url) in check.artefacts() {
                        downloads.push(DownloadArtefact {
                            path: format!("{}{}", mountpoint, path).into(),
                            url,
                        });
                    }
                    pre_download_phase.push(Step::CreateDataset {
                        dataset: dataset.clone(),
                        mountpoint: Some(mountpoint),
                        create_parents: true,
                        chown: chown.clone(),
                    });
                    post_download_phase.push(Step::SetDatasetReadOnly { dataset });
                }
            }
        }
        plan.extend(pre_download_phase);
        plan.push(Step::DownloadArtefacts(downloads));
        plan.extend(post_download_phase);

        // Step 4: Run the dang script.
        plan.push(Step::RunScript {
            script: script.to_owned(),
            workdir,
        });

        // Step 5: Clone and promote /work
        let input = Input::LocalBuild { id: Ulid::new() };
        plan.push(Step::SaveWorkAsInput {
            work_dataset: work,
            new_dataset: format!("{}/{}", OUR_DATASET, input),
            job_name: frontmatter.name,
            input,
        });

        Ok(Plan(plan))
    }

    pub(crate) fn approve(&self) -> Result<bool> {
        eprintln!("this will run the following commands:");
        for step in &self.0 {
            for command in step.commands_for_approval() {
                eprintln!("  {}", command);
            }
        }
        Ok(Confirm::new().with_prompt("continue?").interact()?)
    }

    pub(crate) async fn run(self, client: &Client) -> Result<()> {
        for step in self.0 {
            step.run(client).await?;
        }
        Ok(())
    }
}

fn dataset_exists(dataset: &str) -> Result<bool> {
    Ok(Command::new("zfs")
        .args(["list", dataset])
        .output()?
        .status
        .success())
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: String,
    dependencies: HashMap<String, Dependency>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    job: String,
}

impl FrontMatter {
    fn from_job(path: &Utf8Path) -> Result<FrontMatter> {
        let file = std::fs::read_to_string(path)?;
        let frontmatter = file
            .lines()
            .take_while(|l| l.starts_with('#'))
            .filter(|l| l.starts_with("#:"))
            .map(|l| l.trim_start_matches("#:"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(toml::from_str(&frontmatter)?)
    }
}

#[derive(Debug, Deserialize)]
struct GitHubCheck {
    name: String,
    output: GitHubCheckOutput,
}

#[derive(Debug, Deserialize)]
struct GitHubCheckOutput {
    summary: String,
}

impl GitHubCheck {
    fn artefacts(&self) -> Vec<(String, String)> {
        let arena = Arena::new();
        let root = comrak::parse_document(&arena, &self.output.summary, &ComrakOptions::default());
        root.descendants()
            .filter_map(|node| {
                let NodeValue::Link(ref link) = node.data.borrow().value else { return None };
                let child = node.first_child()?;
                let NodeValue::Code(ref code) = child.data.borrow().value else { return None };
                Some((code.literal.clone(), link.url.clone()))
            })
            .collect()
    }
}
