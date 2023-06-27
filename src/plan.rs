use crate::command::CommandExt;
use crate::input::Input;
use crate::step::{DownloadArtefact, Step};
use crate::{JOB_NAME_PROPERTY, OUR_DATASET, POOL};
use anyhow::{bail, ensure, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use comrak::{nodes::NodeValue, Arena, ComrakOptions};
use dialoguer::Confirm;
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Output, Stdio};
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
        // get the root of the repository.
        let repo = script
            .ancestors()
            .nth(4)
            .context("failed to determine work dir")?
            .to_owned();
        ensure!(
            Some(
                repo.join(".github")
                    .join("buildomat")
                    .join("jobs")
                    .as_path()
            ) == script.parent(),
            "script path not within `.github/buildomat/jobs`"
        );

        let chown = ["-un", "-gn"]
            .into_iter()
            .map(|arg| trim_stdout(&Command::new("id").arg(arg).succeed_output()?))
            .collect::<Result<Vec<_>>>()?
            .join(":");

        let mut plan = Vec::new();

        // Phase 1: Set up rpool/{buildomat-at-home,input,work}

        let mut mounted: HashMap<String, Utf8PathBuf> = HashMap::new();
        if dataset_exists(OUR_DATASET)? {
            let output = Command::new("zfs")
                .args(["list", "-H", "-o", "name,mountpoint", "-r", OUR_DATASET])
                .stderr(Stdio::inherit())
                .succeed_output()?;
            for line in trim_stdout(&output)?.lines() {
                if let Some((dataset, mountpoint)) = line.split_once('\t') {
                    if mountpoint.starts_with("/input") {
                        mounted.insert(dataset.into(), mountpoint.into());
                    }
                }
            }
        } else {
            plan.push(Step::Comment("create rpool/buildomat-at-home".into()));
            plan.push(Step::CreateDataset {
                dataset: OUR_DATASET.into(),
                mountpoint: None,
                create_parents: false,
                chown: chown.clone(),
            });
        }

        let input = format!("{}/input", POOL);
        if !dataset_exists(&input)? {
            plan.push(Step::Comment("create rpool/input (at /input)".into()));
            plan.push(Step::CreateDataset {
                dataset: input,
                mountpoint: Some("/input".into()),
                create_parents: false,
                chown: chown.clone(),
            });
        }

        let work = format!("{}/work", POOL);
        if dataset_exists(&work)? {
            plan.push(Step::Comment("recreate rpool/work (at /work)".into()));
            plan.push(Step::DestroyDataset {
                dataset: work.clone(),
            });
        } else {
            plan.push(Step::Comment("create rpool/work (at /work)".into()));
        }
        plan.push(Step::CreateDataset {
            dataset: work.clone(),
            mountpoint: Some("/work".into()),
            create_parents: false,
            chown: chown.clone(),
        });

        // Phase 2: Set up input mounts and download artifacts

        let mut unmatched = frontmatter
            .dependencies
            .values()
            .map(|v| &v.job)
            .collect::<HashSet<_>>();
        let mut cleanup_phase = Vec::new();
        let mut mount_phase = Vec::new();
        let mut readonly_phase = Vec::new();
        let mut downloads = Vec::new();
        for input in inputs {
            let dataset = format!("{}/{}", OUR_DATASET, input);
            let mut check = None;
            let job_name = match input {
                Input::LocalBuild { .. } => {
                    if let Some(job_name) = dataset_prop(&dataset, JOB_NAME_PROPERTY)? {
                        job_name
                    } else {
                        bail!("input {} not found", input);
                    }
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

            let k = if let Some((k, _)) = frontmatter
                .dependencies
                .iter()
                .find(|(_, v)| v.job == job_name)
            {
                unmatched.remove(&job_name);
                k
            } else {
                bail!("{} is not an input to this job", input);
            };

            let mountpoint = Utf8Path::new("/input").join(k);
            if let Some(check) = check {
                if dataset_exists(&dataset)? {
                    // If `readonly=off`, a previous run was most likely interrupted (since we set
                    // `readonly=on`) after successfully downloading everything.
                    if dataset_prop(&dataset, "readonly")?.as_deref() == Some("off") {
                        mounted.remove(&dataset);
                        cleanup_phase.push(Step::DestroyDataset {
                            dataset: dataset.clone(),
                        });
                    } else {
                        // The input dataset exists and looks fine. Adjust the mountpoint if needed,
                        // but otherwise continue as we do not need to download these artefacts.
                        if mounted.get(&dataset) == Some(&mountpoint) {
                            mounted.remove(&dataset);
                        } else {
                            mount_phase.push(Step::SetDatasetMountpoint {
                                dataset: dataset.clone(),
                                mountpoint,
                            });
                        }
                        continue;
                    }
                }

                for (path, url) in check.artefacts() {
                    downloads.push(DownloadArtefact {
                        path: format!("{}{}", mountpoint, path).into(),
                        url,
                    });
                }
                mount_phase.push(Step::CreateDataset {
                    dataset: dataset.clone(),
                    mountpoint: Some(mountpoint),
                    create_parents: true,
                    chown: chown.clone(),
                });
                readonly_phase.push(Step::SetDatasetReadOnly { dataset });
            }
        }
        ensure!(
            unmatched.is_empty(),
            "inputs {:?} are required but not provided",
            unmatched
        );
        if !mounted.is_empty() {
            plan.push(Step::Comment(
                "remove inputs from a previous job from /input".into(),
            ));
            for (dataset, _) in mounted {
                plan.push(Step::InheritDatasetMountpoint { dataset });
            }
        }
        if !cleanup_phase.is_empty() {
            plan.push(Step::Comment("remove incomplete /input datasets".into()));
            plan.extend(cleanup_phase);
        }
        if !mount_phase.is_empty() {
            plan.push(Step::Comment("set up datasets for /input".into()));
            plan.extend(mount_phase);
        }
        if !downloads.is_empty() {
            plan.push(Step::Comment(format!(
                "download {} artifacts",
                downloads.len()
            )));
            plan.push(Step::DownloadArtefacts(downloads));
        }
        if !readonly_phase.is_empty() {
            plan.push(Step::Comment("mark /input datasets read-only".into()));
            plan.extend(readonly_phase);
        }

        // Phase 3.1: Clone the repository

        let workdir = if frontmatter.skip_clone {
            Utf8PathBuf::from("/work")
        } else {
            let mut treeish = trim_stdout(
                &Command::new("git")
                    .args(["stash", "create"])
                    .current_dir(&repo)
                    .succeed_output()?,
            )?;
            if treeish.is_empty() {
                treeish = trim_stdout(
                    &Command::new("git")
                        .args(["rev-parse", "HEAD"])
                        .current_dir(&repo)
                        .succeed_output()?,
                )?;
            }

            let remote = trim_stdout(
                &Command::new("git")
                    .args(["remote", "get-url", "origin"])
                    .current_dir(&repo)
                    .output()?,
            )?;
            let mut iter = remote.rsplit(['/', ':']);
            let dest = if let (Some(mut repo), Some(owner)) = (iter.next(), iter.next()) {
                repo = repo.strip_suffix(".git").unwrap_or(repo);
                Utf8Path::new("/work").join(owner).join(repo)
            } else {
                Utf8PathBuf::from("/work")
            };

            plan.push(Step::Comment("clone repository into /work".into()));
            plan.push(Step::CloneRepo {
                src: repo,
                treeish,
                dest: dest.clone(),
            });
            dest
        };

        // Phase 3.2: Run the dang script

        plan.push(Step::Comment("run job script".into()));
        plan.push(Step::RunScript {
            script: script.to_owned(),
            workdir,
        });

        // Phase 4: Clone and promote /work

        let input = Input::LocalBuild { id: Ulid::new() };
        plan.push(Step::Comment(format!("save /work as {}", input)));
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

fn trim_stdout(output: &Output) -> Result<String> {
    Ok(std::str::from_utf8(&output.stdout)?.trim().to_owned())
}

fn dataset_exists(dataset: &str) -> Result<bool> {
    Ok(Command::new("zfs")
        .args(["list", dataset])
        .output()?
        .status
        .success())
}

fn dataset_prop(dataset: &str, property: &str) -> Result<Option<String>> {
    let output = Command::new("zfs")
        .args(["get", "-H", "-o", "value", property, dataset])
        .output()?;
    Ok(if output.status.success() {
        Some(trim_stdout(&output)?)
    } else {
        None
    })
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: String,
    #[serde(default)]
    dependencies: HashMap<String, Dependency>,
    #[serde(default)]
    skip_clone: bool,
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
