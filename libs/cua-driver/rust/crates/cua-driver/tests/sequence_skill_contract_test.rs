//! Keep the published sequence example valid under the executable contract.
use cua_driver_contract::RunSequenceInput;
use std::path::PathBuf;

#[test]
fn sequence_skill_example_passes_semantic_validation() {
    let path = std::env::var_os("CUA_SEQUENCE_SKILL_DOC")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Skills/cua-driver/SKILL.md")
        });
    let skill = std::fs::read_to_string(path).expect("sequence skill reference");
    let section = skill
        .split_once("### Verified sequences on one window")
        .expect("sequence reference section")
        .1;
    let example = section
        .split_once("```json\n")
        .expect("sequence JSON example")
        .1
        .split_once("```")
        .expect("closed example")
        .0;
    let request: RunSequenceInput = serde_json::from_str(example).expect("typed sequence example");
    request
        .validate()
        .expect("semantically valid sequence example");
}
