use crate::cli::{
    fixtures::*,
    utils::{regex_pred, FAILURE_LINE},
};
use assert_cmd::Command;
use assert_fs::prelude::*;
use rstest::*;

const FILE_CONTENTS: &str = r#"
some text
on multiple lines
that is a file's contents
"#;

#[rstest]
#[test_log::test]
fn should_output_metadata_for_file(mut action_cmd: CtxCommand<Command>) {
    let temp = assert_fs::TempDir::new().unwrap();

    let file = temp.child("file");
    file.write_str(FILE_CONTENTS).unwrap();

    // distant action metadata {path}
    action_cmd
        .args(["metadata", file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(regex_pred(concat!(
            "Type: file\n",
            "Len: .*\n",
            "Readonly: false\n",
            "Created: .*\n",
            "Last Accessed: .*\n",
            "Last Modified: .*\n",
        )))
        .stderr("");
}

#[rstest]
#[test_log::test]
fn should_output_metadata_for_directory(mut action_cmd: CtxCommand<Command>) {
    let temp = assert_fs::TempDir::new().unwrap();

    let dir = temp.child("dir");
    dir.create_dir_all().unwrap();

    // distant action metadata {path}
    action_cmd
        .args(["metadata", dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(regex_pred(concat!(
            "Type: dir\n",
            "Len: .*\n",
            "Readonly: false\n",
            "Created: .*\n",
            "Last Accessed: .*\n",
            "Last Modified: .*\n",
        )))
        .stderr("");
}

// NOTE: Ignoring on windows because ssh2 doesn't properly canonicalize paths to resolve symlinks!
#[rstest]
#[test_log::test]
#[cfg_attr(windows, ignore)]
fn should_support_including_a_canonicalized_path(mut action_cmd: CtxCommand<Command>) {
    let temp = assert_fs::TempDir::new().unwrap();

    let file = temp.child("file");
    file.touch().unwrap();

    let link = temp.child("link");
    link.symlink_to_file(file.path()).unwrap();

    // distant action metadata --canonicalize {path}
    action_cmd
        .args(["metadata", "--canonicalize", link.to_str().unwrap()])
        .assert()
        .success()
        .stdout(regex_pred(&format!(
            concat!(
                "Canonicalized Path: {:?}\n",
                "Type: symlink\n",
                "Len: .*\n",
                "Readonly: false\n",
                "Created: .*\n",
                "Last Accessed: .*\n",
                "Last Modified: .*\n",
            ),
            file.path().canonicalize().unwrap()
        )))
        .stderr("");
}

#[rstest]
#[test_log::test]
fn should_support_resolving_file_type_of_symlink(mut action_cmd: CtxCommand<Command>) {
    let temp = assert_fs::TempDir::new().unwrap();

    let file = temp.child("file");
    file.touch().unwrap();

    let link = temp.child("link");
    link.symlink_to_file(file.path()).unwrap();

    // distant action metadata --canonicalize {path}
    action_cmd
        .args(["metadata", "--resolve-file-type", link.to_str().unwrap()])
        .assert()
        .success()
        .stdout(regex_pred(concat!(
            "Type: file\n",
            "Len: .*\n",
            "Readonly: false\n",
            "Created: .*\n",
            "Last Accessed: .*\n",
            "Last Modified: .*\n",
        )))
        .stderr("");
}

#[rstest]
#[test_log::test]
fn yield_an_error_when_fails(mut action_cmd: CtxCommand<Command>) {
    let temp = assert_fs::TempDir::new().unwrap();

    // Don't create file
    let file = temp.child("file");

    // distant action metadata {path}
    action_cmd
        .args(["metadata", file.to_str().unwrap()])
        .assert()
        .code(1)
        .stdout("")
        .stderr(FAILURE_LINE.clone());
}
