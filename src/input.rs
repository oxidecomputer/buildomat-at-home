use parse_display::{Display, FromStr};
use ulid::Ulid;

#[derive(Debug, Display, FromStr, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Input {
    #[display("local/{id}")]
    LocalBuild { id: Ulid },
    #[display("github/{owner}/{repo}/{run_id}")]
    #[from_str(
        regex = r"(?:https://)?github(?:\.com)?/(?P<owner>[^/]+)/(?P<repo>[^/]+)/(?:runs/)?(?P<run_id>[^/]+)"
    )]
    GitHubRun {
        owner: String,
        repo: String,
        run_id: String,
    },
}

#[cfg(test)]
#[test]
fn test_from_str() {
    assert_eq!(
        "local/01H3WX25SMVQ9YEDXDDC832VCV".parse::<Input>().unwrap(),
        Input::LocalBuild {
            id: "01H3WX25SMVQ9YEDXDDC832VCV".parse().unwrap()
        }
    );

    let input = Input::GitHubRun {
        owner: "oxidecomputer".into(),
        repo: "omicron".into(),
        run_id: "14561963408".into(),
    };
    assert_eq!(input.to_string().parse::<Input>().unwrap(), input);
    assert_eq!(
        "https://github.com/oxidecomputer/omicron/runs/14561963408"
            .parse::<Input>()
            .unwrap(),
        input
    );
}
