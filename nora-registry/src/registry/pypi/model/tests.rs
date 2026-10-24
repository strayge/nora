// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

use super::*;

const FILE: &str = "pkg-1.0-py3-none-any.whl";
const PUBLIC: &str = "https://nora.example/nora/python/";

fn document() -> Value {
    json!({
        "meta": {"api-version": "1.4"}, "name": "Pkg", "versions": ["0.9", "1.0", "2.0"],
        "files": [{"filename": FILE, "url": format!("../../files/{FILE}?token=a&b=c"),
            "hashes": {"sha256": "ab".repeat(32), "blake2b": "cd".repeat(64)},
            "requires-python": ">=3.8,<4", "yanked": "broken <ABI> & \"reason\"",
            "core-metadata": {"sha256": "ef".repeat(32), "sha512": "12".repeat(64)},
            "size": 123, "upload-time": "2024-01-02T03:04:05.123456+02:00"}],
        "project-status": {"status":"archived", "reason":"old <project> & \"reason\""}
    })
}

fn parsed(document: &Value) -> Project {
    parse_json(
        &serde_json::to_vec(document).unwrap(),
        "pkg",
        &Url::parse("https://source.example/api/simple/pkg/").unwrap(),
    )
    .unwrap()
}

fn rendered(project: &Project, files: &[Distribution], full: bool, sidecars: bool) -> Value {
    serde_json::from_slice(&render_json(project, files, PUBLIC, full, sidecars, None).unwrap())
        .unwrap()
}

#[test]
fn rich_json_preserves_constraints_hashes_versions_and_real_times_without_source_urls() {
    let project = parsed(&document());
    let file = &project.files[0];
    assert_eq!(
        file.url,
        format!("https://source.example/api/files/{FILE}?token=a&b=c")
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.8,<4"));
    assert_eq!(
        file.upload_time.as_deref(),
        Some("2024-01-02T01:04:05.123456Z")
    );
    assert_eq!(file.hashes.len(), 2);
    assert!(
        matches!(file.core_metadata, Some(CoreMetadata::Hashes(ref hashes)) if hashes.len() == 2)
    );
    let output = rendered(&project, &project.files, true, false);
    assert_eq!(output["meta"]["api-version"], "1.4");
    assert_eq!(output["versions"], json!(["0.9", "1.0", "2.0"]));
    assert_eq!(output["project-status"]["status"], "archived");
    assert_eq!(output["files"][0]["url"], format!("{PUBLIC}pkg/{FILE}"));
    assert_eq!(output["files"][0]["hashes"]["blake2b"], "cd".repeat(64));
    assert!(output["files"][0].get("core-metadata").is_none());
    assert!(!output.to_string().contains("source.example"));
}

#[test]
fn html5_parsing_decodes_entities_and_ignores_markup_inside_scripts() {
    let html = format!(
        r#"<!doctype html><HTML><HEAD>
<meta name="pypi:project-status" content="deprecated">
<meta name="pypi:project-status-reason" content="&copy; &quot;reason&quot;">
<base href="https://cdn.example/files/">
<script>const fake='<a href="pkg-9.0.whl">fake</a>';</script>
</HEAD><BODY><P>unclosed paragraph
<A HREF="pkg-1.0%2Bcpu-py3-none-any.whl?one=1&amp;two=2#sha256=abab"
DATA-REQUIRES-PYTHON="&gt;=3.10,&lt;4" data-yanked data-dist-info-metadata>
file</A></BODY></HTML>"#
    );
    let project = parse_html(
        html.as_bytes(),
        "pkg",
        &Url::parse("https://source.example/simple/pkg/").unwrap(),
    )
    .unwrap();
    assert_eq!(project.files.len(), 1);
    let file = &project.files[0];
    assert_eq!(file.filename, "pkg-1.0+cpu-py3-none-any.whl");
    assert_eq!(
        file.url,
        "https://cdn.example/files/pkg-1.0%2Bcpu-py3-none-any.whl?one=1&two=2"
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.10,<4"));
    assert!(matches!(file.yanked, Some(Yanked::Flag(true))));
    assert!(file.core_metadata.as_ref().unwrap().available());
    assert_eq!(project.status.reason.as_deref(), Some("© \"reason\""));
    assert!(matches!(project.status.status, Status::Deprecated));
    let output = rendered(&project, &project.files, true, false);
    assert_eq!(output["files"][0]["yanked"], true);
    assert_eq!(output["meta"]["api-version"], "1.0");
}

#[test]
fn unknown_sizes_downgrade_only_the_response_not_the_rich_internal_metadata() {
    let mut project = parsed(&document());
    project.files[0].size = None;
    let output = rendered(&project, &project.files, true, false);
    assert_eq!(output["meta"]["api-version"], "1.0");
    for key in ["versions", "project-status"] {
        assert!(output.get(key).is_none());
    }
    for key in ["size", "upload-time", "core-metadata"] {
        assert!(output["files"][0].get(key).is_none());
    }
    assert_eq!(output["files"][0]["requires-python"], ">=3.8,<4");
    assert!(project.files[0].upload_time.is_some());
    assert!(project.files[0].core_metadata.is_some());
    assert_eq!(project.versions.as_ref().unwrap().len(), 3);
}

#[test]
fn html_output_escapes_metadata_and_suppresses_unsupported_sidecars() {
    let project = parsed(&document());
    let html = render_html(
        &project,
        &project.files,
        "https://nora.example/a&b/",
        false,
        None,
    );
    assert!(html.contains("https://nora.example/a&amp;b/pkg/"));
    assert!(html.contains("data-requires-python=\"&gt;=3.8,&lt;4\""));
    assert!(html.contains("data-yanked=\"broken &lt;ABI&gt; &amp; &quot;reason&quot;\""));
    assert!(html.contains("old &lt;project&gt; &amp; &quot;reason&quot;"));
    assert!(!html.contains("data-core-metadata"));
    assert!(!html.contains("data-dist-info-metadata"));
    assert!(!html.contains("source.example"));
}

#[test]
fn core_metadata_aliases_have_canonical_precedence_and_keep_all_hashes() {
    let mut input = document();
    input["files"][0]["core-metadata"] = json!(false);
    input["files"][0]["dist-info-metadata"] = json!(true);
    let project = parsed(&input);
    assert!(!project.files[0].core_metadata.as_ref().unwrap().available());
    input["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("core-metadata");
    let project = parsed(&input);
    assert!(project.files[0].core_metadata.as_ref().unwrap().available());
    input["files"][0]["core-metadata"] =
        json!({"sha256":"ab".repeat(32), "sha512":"cd".repeat(64)});
    let project = parsed(&input);
    let output = rendered(&project, &project.files, true, true);
    assert_eq!(
        output["files"][0]["core-metadata"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn yanked_boolean_reason_and_absence_remain_distinct() {
    for value in [
        None,
        Some(json!(false)),
        Some(json!(true)),
        Some(json!("reason")),
        Some(json!("")),
    ] {
        let mut input = document();
        input["files"][0].as_object_mut().unwrap().remove("yanked");
        if let Some(value) = &value {
            input["files"][0]["yanked"] = value.clone();
        }
        let project = parsed(&input);
        let output = rendered(&project, &project.files, true, false);
        if let Some(value) = value {
            assert_eq!(
                output["files"][0]["yanked"],
                if value == json!("") {
                    json!(true)
                } else {
                    value
                }
            );
        } else {
            assert!(output["files"][0].get("yanked").is_none());
        }
    }
}

#[test]
fn project_status_never_loses_quarantine_during_json_downgrade() {
    for status in ["active", "archived", "deprecated", "quarantined"] {
        let mut input = document();
        input["project-status"]["status"] = json!(status);
        let mut project = parsed(&input);
        let output = rendered(&project, &project.files, true, false);
        assert_eq!(output["project-status"]["status"], status);
        if status == "quarantined" {
            assert!(output["files"].as_array().unwrap().is_empty());
            assert!(
                !render_html(&project, &project.files, PUBLIC, false, None).contains("<a href=")
            );
            project.files[0].size = None;
            let output = rendered(&project, &project.files, true, false);
            assert_eq!(output["meta"]["api-version"], "1.0");
            assert!(output["files"].as_array().unwrap().is_empty());
            assert!(output.get("project-status").is_none());
        }
    }
}

#[test]
fn filtered_versions_do_not_advertise_uncached_releases_and_bindings_survive_sidecar_suffixes() {
    let project = parsed(&document());
    let output = rendered(&project, &project.files, false, false);
    assert_eq!(output["versions"], json!(["1.0"]));
    let url = file_url(PUBLIC, "pkg", FILE, Some("member:source"));
    assert_eq!(url, format!("{PUBLIC}-/source/member%3Asource/pkg/{FILE}"));
    let sidecar = Url::parse(&format!("{url}.metadata")).unwrap();
    assert_eq!(sidecar.query(), None);
    assert!(sidecar.path().ends_with(".whl.metadata"));
}

#[test]
fn authoritative_versions_keep_their_spelling_for_legacy_equivalent_filenames() {
    let mut input = document();
    input["versions"] = json!(["1.0.post1", "2.0"]);
    input["files"][0]["filename"] = json!("pkg-1.0-1.tar.gz");
    let project = parsed(&input);
    assert_eq!(
        rendered(&project, &project.files, true, false)["versions"],
        json!(["1.0.post1", "2.0"])
    );
    assert_eq!(
        rendered(&project, &project.files, false, false)["versions"],
        json!(["1.0.post1"])
    );
}

/// Historical uploads can belong to a release that differs from the filename's version.
#[test]
fn legacy_release_versions_need_not_match_distribution_filenames() {
    let mut input = document();
    input["versions"] = json!(["0.2"]);
    input["files"][0]["filename"] = json!("pkg-0.3.tar.gz");
    input["files"][0]["url"] = json!("../../files/pkg-0.3.tar.gz");
    input["project-status"] = json!({"status": "active"});
    let project = parsed(&input);
    let output = rendered(&project, &project.files, true, false);
    assert_eq!(output["versions"], json!(["0.2"]));
    assert_eq!(output["files"][0]["filename"], "pkg-0.3.tar.gz");
    assert_eq!(
        rendered(&project, &project.files, false, false)["versions"],
        json!(["0.3"])
    );

    input["project-status"]["status"] = json!("quarantined");
    let project = parsed(&input);
    assert!(rendered(&project, &project.files, true, false)["files"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn malformed_structured_metadata_is_not_package_absence() {
    for replacement in [
        json!({"meta":{"api-version":"2.0"}}),
        json!({"name":"another"}),
        json!({"project-status":{"status":"unknown"}}),
    ] {
        let mut input = document();
        for (key, value) in replacement.as_object().unwrap() {
            input[key] = value.clone();
        }
        assert!(parse_json(
            &serde_json::to_vec(&input).unwrap(),
            "pkg",
            &Url::parse(PUBLIC).unwrap()
        )
        .is_err());
    }
    let mut input = document();
    input["files"][0]["hashes"] = json!({"SHA256":"ab", "sha256":"ab"});
    assert!(parse_json(
        &serde_json::to_vec(&input).unwrap(),
        "pkg",
        &Url::parse(PUBLIC).unwrap()
    )
    .is_err());
    input["files"][0]["hashes"] = json!({});
    input["files"][0]["url"] = json!("javascript:alert(1)");
    assert!(parse_json(
        &serde_json::to_vec(&input).unwrap(),
        "pkg",
        &Url::parse(PUBLIC).unwrap()
    )
    .is_err());
    let empty = json!({"meta":{"api-version":"1.0"},"name":"pkg","files":[]});
    assert!(parsed(&empty).files.is_empty());
}
