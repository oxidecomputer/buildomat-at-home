#![warn(clippy::pedantic)]
#![allow(clippy::uninlined_format_args)]

mod input;
mod plan;
mod step;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use reqwest::Client;
use std::str::FromStr;

const POOL: &str = "rpool";
const OUR_DATASET: &str = "rpool/buildomat-at-home";
const JOB_NAME_PROPERTY: &str = "computer.oxide.eng.buildomat-at-home:job_name";

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::builder()
        .user_agent("https://github.com/oxidecomputer/buildomat-at-home")
        .build()?;

    let mut args = std::env::args().skip(1);
    let script = match args.next() {
        Some(x) => Utf8PathBuf::from(x)
            .canonicalize_utf8()
            .context("failed to canonicalize job script path")?,
        None => bail!("no job script specified\nusage: buildomat-at-home SCRIPT [INPUTS...]"),
    };

    let mut inputs = Vec::new();
    for arg in args {
        inputs.push(input::Input::from_str(&arg)?);
    }
    inputs.sort_unstable();

    let plan = plan::Plan::build(&client, &script, &inputs).await?;
    if plan.approve()? {
        plan.run(&client).await?;
    }

    Ok(())
}