use lib::testing::{make_git, GitRunOptions};

#[test]
fn test_test() -> eyre::Result<()> {
    let git = make_git()?;

    git.init_repo()?;
    git.detach_head()?;
    git.commit_file("test2", 2)?;
    git.commit_file("test3", 3)?;

    {
        let (stdout, _stderr) = git.run(&["test", "-c", "exit 0"])?;
        insta::assert_snapshot!(stdout, @r###"
        branchless: running command: <git-executable> diff --quiet
        Calling Git for on-disk rebase...
        branchless: running command: <git-executable> rebase --continue
        branchless: running command: <git-executable> checkout fe65c1fe15584744e649b2c79d4cf9b0d878f92e
        branchless: running command: <git-executable> checkout 02067177964ab16eedc74600341b2d9e4e19487e
        Ran exit 0 on 2 commits:
        ✔️ Passed: fe65c1f create test2.txt
        ✔️ Passed: 0206717 create test3.txt
        1 passed, 0 failed, 0 skipped
        branchless: running command: <git-executable> rebase --abort
        "###);
    }

    {
        let (stdout, _stderr) = git.run_with_options(
            &["test", "-c", "exit 1"],
            &GitRunOptions {
                expected_exit_code: 1,
                ..Default::default()
            },
        )?;
        insta::assert_snapshot!(stdout, @r###"
        branchless: running command: <git-executable> diff --quiet
        Calling Git for on-disk rebase...
        branchless: running command: <git-executable> rebase --continue
        branchless: running command: <git-executable> checkout fe65c1fe15584744e649b2c79d4cf9b0d878f92e
        branchless: running command: <git-executable> checkout 02067177964ab16eedc74600341b2d9e4e19487e
        Ran exit 1 on 2 commits:
        ✖️ Failed with exit code 1: fe65c1f create test2.txt
        ✖️ Failed with exit code 1: 0206717 create test3.txt
        0 passed, 2 failed, 0 skipped
        branchless: running command: <git-executable> rebase --abort
        "###);
    }

    Ok(())
}
