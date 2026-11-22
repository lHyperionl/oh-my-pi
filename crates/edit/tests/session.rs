//! Session-level contracts: streamed previews converge on the applied diff,
//! sloppy multi-file payloads fail only when no file changes, and apply never
//! trusts preview reads.

mod common;

use common::{DiskWriter, Workspace};
use omp_edit::{
	ApplyRequest, EditError, EditMode, FileOp, HostedKey, UrlAnswer, session::PreviewBatch,
};

fn hosted_key(url: &str) -> HostedKey {
	HostedKey::new(url.to_owned()).unwrap()
}

fn remote_answer(url: &str, content: Option<&str>) -> UrlAnswer {
	UrlAnswer::Hosted {
		key:           hosted_key(url),
		content:       content.map(str::to_owned),
		plan_writable: false,
	}
}

#[derive(Default)]
struct HostedWriter {
	requests: parking_lot::Mutex<Vec<omp_edit::WriteRequest>>,
}

#[async_trait::async_trait]
impl omp_edit::EditWriter for HostedWriter {
	async fn write(
		&self,
		request: omp_edit::WriteRequest,
	) -> omp_edit::EditResult<omp_edit::WriteResponse> {
		let written = request.content.clone().unwrap_or_default();
		self.requests.lock().push(request);
		Ok(omp_edit::WriteResponse { written, diagnostics_json: None })
	}
}

#[tokio::test]
async fn hosted_patch_uses_typed_writes_and_preserves_opaque_keys() {
	let first = "ssh://host/a//note.txt";
	let second = "ssh://host/a/note.txt";
	let mut ws = Workspace::new(EditMode::ApplyPatch);
	ws.config.policy.url_schemes = vec!["ssh".into()];
	let mut session = ws.session();
	let input = format!(
		"*** Begin Patch\n*** Update File: {first}\n@@\n-first\n+FIRST\n*** Update File: \
		 {second}\n@@\n-second\n+SECOND\n*** End Patch"
	);
	session.set_args_json(&serde_json::json!({ "input": input }).to_string());
	session.finish();
	assert_eq!(session.begin_apply_url_targets(), [first, second]);
	session.provide(first.into(), remote_answer(first, Some("first\n")));
	session.provide(second.into(), remote_answer(second, Some("second\n")));
	let writer = HostedWriter::default();
	let outcome = session
		.apply(ApplyRequest::default(), &writer)
		.await
		.unwrap();
	let requests = writer.requests.lock();
	assert_eq!(requests.len(), 2);
	assert_eq!(requests[0].absolute.to_str(), Some(first));
	assert_eq!(requests[0].content.as_deref(), Some("FIRST\n"));
	assert_eq!(requests[1].absolute.to_str(), Some(second));
	assert_eq!(requests[1].content.as_deref(), Some("SECOND\n"));
	assert!(requests.iter().all(|request| request.hosted));
	assert!(outcome.files.iter().all(|file| file.hosted));
}

#[tokio::test]
async fn patch_moves_require_the_same_edit_store() {
	let remote = "ssh://host/from.txt";
	let same = "ssh://host/to.txt";
	let other = "ssh://other/to.txt";
	for (from, to, allowed) in [
		("from.txt", "to.txt", true),
		(remote, same, true),
		("from.txt", same, false),
		(remote, "to.txt", false),
		(remote, other, false),
	] {
		let mut ws = Workspace::new(EditMode::ApplyPatch);
		ws.config.policy.url_schemes = vec!["ssh".into()];
		ws.write("from.txt", "before\n");
		let mut session = ws.session();
		let input = format!(
			"*** Begin Patch\n*** Update File: {from}\n*** Move to: {to}\n@@\n-before\n+after\n*** \
			 End Patch"
		);
		session.set_args_json(&serde_json::json!({ "input": input }).to_string());
		session.finish();
		for url in session.begin_apply_url_targets() {
			let content = (url == remote).then_some("before\n");
			session.provide(url.clone(), remote_answer(&url, content));
		}
		let writer = HostedWriter::default();
		let result = session.apply(ApplyRequest::default(), &writer).await;
		let requests = writer.requests.lock();
		if allowed {
			let outcome = result.unwrap();
			assert_eq!(requests.len(), 1);
			assert_eq!(requests[0].hosted, from == remote);
			assert_eq!(outcome.files[0].hosted, from == remote);
			assert!(requests[0].move_to.is_some());
		} else {
			assert!(
				result
					.unwrap_err()
					.to_string()
					.contains("Cannot move between edit stores")
			);
			assert!(requests.is_empty());
		}
	}
}

#[test]
fn hosted_answers_refresh_aliases_without_touching_disk_reads() {
	use omp_edit::files::{FileCache, FileSource};
	let mut ws = Workspace::new(EditMode::Replace);
	ws.config.policy.url_schemes = vec!["ssh".into()];
	ws.write("local.txt", "disk\n");
	let mut files = FileCache::new(ws.config.policy.clone());
	let key = "ssh://host/a.ipynb";
	let alias = "ssh://host/~/a.ipynb";
	files.provide(key.into(), remote_answer(key, Some("old\n")));
	let disk = files.read("local.txt").unwrap();
	assert_eq!(files.read(key).unwrap().text, "old\n");
	files.provide(alias.into(), remote_answer(key, Some("new\n")));
	let read = files.read(key).unwrap();
	assert_eq!(read.text, "new\n");
	assert!(!read.is_notebook);
	assert_eq!(read.canonical, omp_edit::Location::Hosted(hosted_key(key)));
	assert!(std::sync::Arc::ptr_eq(&disk, &files.read("local.txt").unwrap()));
}

#[tokio::test]
async fn hosted_create_keeps_notebooks_as_raw_json() {
	let mut ws = Workspace::new(EditMode::ApplyPatch);
	ws.config.policy.url_schemes = vec!["hosted".into()];
	let mut session = ws.session();
	session.set_args_json(r#"{"input":"*** Begin Patch\n*** Add File: hosted://account/new.ipynb\n+{\"cells\":[]}\n*** End Patch"}"#);
	session.finish();
	session.begin_apply_url_targets();
	let key = "hosted://account/new.ipynb";
	session.provide(key.into(), remote_answer(key, None));
	let writer = HostedWriter::default();
	session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("hosted create");
	let requests = writer.requests.lock();
	assert_eq!(requests[0].absolute.to_str(), Some(key));
	assert_eq!(requests[0].content.as_deref(), Some("{\"cells\":[]}\n"));
}

#[tokio::test]
async fn remote_apply_uses_fresh_host_text_even_when_its_length_is_unchanged() {
	let url = "ssh://oracle/tmp/edit-repro.txt";
	let mut ws = Workspace::new(EditMode::Replace);
	ws.config.policy.url_schemes = vec!["ssh".into()];
	let mut session = ws.session();
	session.set_args_json(
		&serde_json::json!({ "path": url, "old_string": "beta", "new_string": "BETA" }).to_string(),
	);
	session.finish();
	session.provide(url.into(), remote_answer(url, Some("alpha\nbeta\n")));
	assert!(session.preview().files[0].diff.is_some());
	assert_eq!(session.begin_apply_url_targets(), [url]);
	session.provide(url.into(), remote_answer(url, Some("delta\nbeta\n")));
	let writer = HostedWriter::default();
	session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("remote update");
	let requests = writer.requests.lock();
	assert_eq!(requests[0].absolute.to_str(), Some(url));
	assert_eq!(requests[0].content.as_deref(), Some("delta\nBETA\n"));
}

#[test]
fn streaming_apply_patch_never_queues_disk_backed_urls() {
	let url = "local://plan.md";
	let mut ws = Workspace::new(EditMode::ApplyPatch);
	ws.config.policy.url_schemes = vec!["local".into()];
	let mut session = ws.session();
	let input = format!("*** Begin Patch\n*** Add File: {url}\n+plan\n");
	session.set_args_json(&serde_json::json!({ "input": input }).to_string());
	let preview = session.preview();
	assert!(preview.streaming);
	assert_eq!(preview.files.len(), 1, "{preview:?}");
	assert_eq!(preview.files[0].display, url);
	assert!(session.take_unresolved().is_empty());
	session.provide(url.into(), UrlAnswer::Pending);
	assert!(session.preview().files.is_empty());
	assert!(session.take_unresolved().is_empty());
}

#[test]
fn streaming_patch_omits_pending_targets_and_moves_to_pending_destinations() {
	let url = "ssh://host/file.txt";
	let destination = "ssh://host/destination.txt";
	let mut ws = Workspace::new(EditMode::ApplyPatch);
	ws.config.policy.url_schemes = vec!["ssh".into()];
	let mut session = ws.session();
	let input = format!(
		"*** Begin Patch\n*** Update File: {url}\n@@\n-old\n+new\n*** Update File: move.txt\n*** \
		 Move to: {destination}\n@@\n-before\n+after\n*** Update File: local.txt\n@@\n-old\n+local\n"
	);
	session.set_args_json(&serde_json::json!({ "input": input }).to_string());
	assert_eq!(session.preview().files.len(), 3);
	assert!(session.take_unresolved().is_empty());
	session.provide(url.into(), UrlAnswer::Pending);
	session.provide(destination.into(), UrlAnswer::Pending);
	let preview = session.preview();
	assert_eq!(preview.files.len(), 1, "{preview:?}");
	assert_eq!(preview.files[0].display, "local.txt");
	assert!(session.take_unresolved().is_empty());
}

#[tokio::test]
async fn apply_patch_deleted_target_is_not_revived_by_a_later_section() {
	let ws = Workspace::new(EditMode::ApplyPatch);
	ws.write("a.txt", "old\n");
	ws.write("b.txt", "before\n");
	let writer = DiskWriter::default();
	let error = ws
		.apply_raw(
			"*** Begin Patch\n*** Delete File: a.txt\n*** Update File: \
			 b.txt\n@@\n-before\n+after\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
			&writer,
		)
		.await
		.expect_err("a staged deletion must remain a tombstone");
	assert!(error.to_string().contains("File not found"), "{error}");
	assert!(writer.requests.lock().is_empty());
	assert_eq!(ws.read("a.txt").as_deref(), Some("old\n"));
	assert_eq!(ws.read("b.txt").as_deref(), Some("before\n"));
}

#[tokio::test]
async fn final_apply_patch_preview_stops_at_the_first_pending_section() {
	let url = "ssh://host/remote.txt";
	let mut ws = Workspace::new(EditMode::ApplyPatch);
	ws.config.policy.url_schemes = vec!["ssh".into()];
	ws.write("first.txt", "one\n");
	ws.write("last.txt", "three\n");
	let input = format!(
		"*** Begin Patch\n*** Update File: first.txt\n@@\n-one\n+ONE\n*** Update File: \
		 {url}\n@@\n-two\n+TWO\n*** Update File: last.txt\n@@\n-three\n+THREE\n*** End Patch"
	);
	let mut session = ws.session();
	session.set_args_json(&serde_json::json!({ "input": input }).to_string());
	session.finish();
	session.provide(url.into(), UrlAnswer::Pending);
	let preview = session.preview();
	assert_eq!(preview.files.len(), 1, "{preview:?}");
	assert_eq!(preview.files[0].display, "first.txt");
	session.provide(url.into(), remote_answer(url, Some("two\n")));
	let preview = session.preview();
	assert_eq!(preview.files.len(), 3, "{preview:?}");

	assert_eq!(session.begin_apply_url_targets(), [url]);
	session.provide(url.into(), UrlAnswer::Pending);
	let writer = HostedWriter::default();
	assert!(
		session
			.apply(ApplyRequest::default(), &writer)
			.await
			.unwrap_err()
			.is_deferred()
	);
	assert!(writer.requests.lock().is_empty());
}

const SOURCE: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";

fn sloppy_payload(path: &str) -> String {
	format!("*** Edit File: {path}\n*** Find\n    let x = 1;\n*** Replace\n    let x = 2;\n")
}

#[tokio::test]
async fn streamed_sloppy_previews_grow_and_match_apply() {
	let ws = Workspace::new(EditMode::Sloppy);
	ws.write("a.rs", SOURCE);
	let payload = sloppy_payload("a.rs");
	let json = serde_json::json!({ "input": payload }).to_string();

	let mut session = ws.session();
	let mut batches: Vec<PreviewBatch> = Vec::new();
	for ch in json.chars() {
		session.push(&ch.to_string());
		let batch = session.preview();
		assert!(batch.streaming);
		assert!(
			batch.files.iter().all(|f| f.error.is_none()),
			"no transient errors while streaming: {batch:?}"
		);
		if !batch.files.is_empty() {
			batches.push(batch);
		}
	}
	assert!(!batches.is_empty(), "streaming produced previews");
	let last = batches.last().unwrap();
	let streamed = last.files[0].diff.as_deref().unwrap_or("");
	assert!(streamed.contains("+2|    let x = 2;"), "streamed diff shows the addition: {streamed}");

	session.finish();
	let final_batch = session.preview();
	assert!(!final_batch.streaming);
	let final_diff = final_batch.files[0].diff.clone().expect("final diff");
	assert!(!session.preview_pending());

	let writer = DiskWriter::default();
	let outcome = session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("apply");
	assert_eq!(outcome.files.len(), 1);
	assert_eq!(outcome.files[0].diff, final_diff);
	assert_eq!(ws.read("a.rs").unwrap(), SOURCE.replace("let x = 1;", "let x = 2;"));
}

#[tokio::test]
async fn multi_file_failure_names_every_file_when_none_changes() {
	let ws = Workspace::new(EditMode::Sloppy);
	ws.write("a.rs", SOURCE);
	ws.write("b.rs", "fn other() {}\n");
	let miss = |path: &str| {
		format!(
			"*** Edit File: {path}\n*** Find\nfn missing() {{}}\n*** Replace\nfn present() {{}}\n"
		)
	};
	let writer = DiskWriter::default();
	let err = ws
		.apply_raw(&format!("{}{}", miss("a.rs"), miss("b.rs")), &writer)
		.await
		.expect_err("no section changes anything");
	let message = err.to_string();
	assert_eq!(err.kind(), "no_match", "{message}");
	assert!(message.starts_with("[a.rs]: "), "{message}");
	assert!(message.contains("\n\n[b.rs]: "), "{message}");
	assert!(writer.requests.lock().is_empty());
	assert_eq!(ws.read("a.rs").unwrap(), SOURCE);
}

#[tokio::test]
async fn a_file_failure_keeps_its_kind_beside_another_file() {
	for mode in [EditMode::ApplyPatch, EditMode::Sloppy] {
		let ws = Workspace::new(mode);
		ws.write("a.rs", SOURCE);
		std::fs::write(ws.cwd().join("bad.rs"), b"fn caf\xe9() {}\n").unwrap();
		let section = |path: &str| match mode {
			EditMode::ApplyPatch => {
				format!("*** Update File: {path}\n@@\n-    let x = 1;\n+    let x = 2;\n")
			},
			_ => sloppy_payload(path),
		};
		let payload = |body: String| match mode {
			EditMode::ApplyPatch => format!("*** Begin Patch\n{body}*** End Patch"),
			_ => body,
		};
		for (target, kind) in [("missing.rs", "file_not_found"), ("bad.rs", "invalid_utf8")] {
			let alone = ws
				.apply_raw(&payload(section(target)), &DiskWriter::default())
				.await
				.expect_err(target);
			let writer = DiskWriter::default();
			let beside = ws
				.apply_raw(&payload(format!("{}{}", section("a.rs"), section(target))), &writer)
				.await
				.expect_err(target);
			assert_eq!(alone.kind(), kind, "{mode:?}: {alone}");
			assert_eq!(beside.kind(), kind, "{mode:?}: {beside}");
			assert!(beside.to_string().starts_with(&format!("[{target}]: ")), "{beside}");
			assert!(writer.requests.lock().is_empty());
		}
	}
}

#[tokio::test]
async fn apply_rereads_files_changed_after_preview() {
	let ws = Workspace::new(EditMode::Replace);
	ws.write("a.txt", "alpha\nbeta\ngamma\n");
	let mut session = ws.session();
	session.set_args_json(
		&serde_json::json!({ "path": "a.txt", "old_string": "beta", "new_string": "BETA" })
			.to_string(),
	);
	let preview = session.preview();
	assert!(
		preview.files[0]
			.diff
			.as_deref()
			.unwrap()
			.contains("+2|BETA")
	);

	ws.write("a.txt", "zero\nalpha\nbeta\ngamma\n");
	session.finish();
	let writer = DiskWriter::default();
	let outcome = session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("apply");
	assert!(outcome.files[0].diff.contains("+3|BETA"), "{}", outcome.files[0].diff);
	assert_eq!(ws.read("a.txt").unwrap(), "zero\nalpha\nBETA\ngamma\n");
}

#[tokio::test]
async fn invalid_utf8_is_rejected_without_rewriting_unrelated_bytes() {
	let ws = Workspace::new(EditMode::Replace);
	let path = ws.cwd().join("legacy.txt");
	let original = b"name=caf\xe9\nalpha\n";
	std::fs::write(&path, original).unwrap();
	let writer = DiskWriter::default();
	let result = ws
		.apply_json(
			&serde_json::json!({ "path": "legacy.txt", "old_string": "alpha", "new_string": "beta" }),
			&writer,
		)
		.await;
	assert_eq!(std::fs::read(&path).unwrap(), original);
	let error = result.expect_err("invalid UTF-8 must be rejected");
	let message = error.to_string();
	assert!(message.contains("legacy.txt"), "{message}");
	assert!(message.contains("byte 8"), "{message}");
	assert!(writer.requests.lock().is_empty());
}

#[tokio::test]
async fn delete_removes_invalid_utf8_file_without_decoding() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("legacy.bin");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	let outcome = ws
		.apply_json(
			&serde_json::json!({ "path": "legacy.bin", "edits": [{ "op": "delete" }] }),
			&writer,
		)
		.await
		.expect("delete needs existence, not text");
	assert!(!path.exists(), "invalid UTF-8 file must be deletable");
	assert_eq!(writer.requests.lock().len(), 1);
	assert!(outcome.text.contains("Deleted legacy.bin"), "{}", outcome.text);
}

#[tokio::test]
async fn create_over_invalid_utf8_reports_already_exists() {
	let ws = Workspace::new(EditMode::ApplyPatch);
	let path = ws.cwd().join("legacy.txt");
	let original = b"name=caf\xe9\n";
	std::fs::write(&path, original).unwrap();
	let writer = DiskWriter::default();
	let err = ws
		.apply_raw("*** Begin Patch\n*** Add File: legacy.txt\n+new\n*** End Patch", &writer)
		.await
		.expect_err("existing undecodable file still exists");
	assert!(err.to_string().contains("already exists"), "{err}");
	assert_eq!(std::fs::read(&path).unwrap(), original);
	assert!(writer.requests.lock().is_empty());
}

#[tokio::test]
async fn patch_create_overwrite_preserves_generated_file_guard() {
	for (name, original) in [
		("schema.gen.ts", b"const value = 1;\n".as_slice()),
		("source.ts", b"// @generated\nconst value = 1;\n".as_slice()),
		("legacy.ts", b"// @generated\nname=caf\xe9\n".as_slice()),
	] {
		let ws = Workspace::new(EditMode::Patch);
		let path = ws.cwd().join(name);
		std::fs::write(&path, original).unwrap();
		let mut session = ws.session();
		session.set_args_json(
			&serde_json::json!({ "path": name, "edits": [{ "op": "create", "diff": "+new" }] })
				.to_string(),
		);
		session.finish();
		let preview = session.preview();
		let writer = DiskWriter::default();
		let result = session.apply(ApplyRequest::default(), &writer).await;
		assert_eq!(std::fs::read(&path).unwrap(), original, "{name}");
		let err = result.expect_err("create-overwrite must respect generated-file protection");
		assert!(err.to_string().contains("auto-generated"), "{name}: {err}");
		assert!(writer.requests.lock().is_empty(), "{name}");
		assert!(
			preview.files[0]
				.error
				.as_deref()
				.is_some_and(|error| error.contains("auto-generated")),
			"{name}: {preview:?}"
		);
	}
}

#[tokio::test]
async fn patch_create_overwrites_invalid_utf8_when_policy_allows_it() {
	for (block_auto_generated, original) in
		[(true, b"name=caf\xe9\n".as_slice()), (false, b"// @generated\nname=caf\xe9\n".as_slice())]
	{
		let mut ws = Workspace::new(EditMode::Patch);
		ws.config.policy.block_auto_generated = block_auto_generated;
		let path = ws.cwd().join("legacy.ts");
		std::fs::write(&path, original).unwrap();
		let writer = DiskWriter::default();
		ws.apply_json(
			&serde_json::json!({ "path": "legacy.ts", "edits": [{ "op": "create", "diff": "+new" }] }),
			&writer,
		)
		.await
		.expect("whole-file replacement does not require decoding");
		assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
	}
}

#[tokio::test]
async fn delete_then_create_replaces_invalid_utf8_file() {
	let ws = Workspace::new(EditMode::ApplyPatch);
	let path = ws.cwd().join("a.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	ws.apply_raw(
		"*** Begin Patch\n*** Delete File: a.txt\n*** Add File: a.txt\n+new\n*** End Patch",
		&writer,
	)
	.await
	.expect("delete+create needs existence, not text");
	assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
	assert_eq!(writer.requests.lock().len(), 1);
}

#[tokio::test]
async fn delete_create_update_uses_replacement_text_for_invalid_utf8_file() {
	let ws = Workspace::new(EditMode::ApplyPatch);
	let path = ws.cwd().join("legacy.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	ws.apply_raw(
		"*** Begin Patch\n*** Delete File: legacy.txt\n*** Add File: legacy.txt\n+new\n*** Update \
		 File: legacy.txt\n@@\n-new\n+updated\n*** End Patch",
		&writer,
	)
	.await
	.expect("update must use newly created text");
	assert_eq!(std::fs::read(&path).unwrap(), b"updated\n");
	assert_eq!(writer.requests.lock().len(), 1);
}

#[tokio::test]
async fn create_update_uses_replacement_text_for_invalid_utf8_file() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("legacy.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	ws.apply_json(
		&serde_json::json!({ "path": "legacy.txt", "edits": [
			{ "op": "create", "diff": "+new" },
			{ "op": "update", "diff": "@@\n-new\n+updated" }
		] }),
		&writer,
	)
	.await
	.expect("update must use replacement text");
	assert_eq!(std::fs::read(&path).unwrap(), b"updated\n");
	assert_eq!(writer.requests.lock().len(), 1);
}

#[tokio::test]
async fn update_before_create_rejects_invalid_utf8_without_writing() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("legacy.txt");
	let original = b"name=caf\xe9\nalpha\n";
	std::fs::write(&path, original).unwrap();
	let writer = DiskWriter::default();
	let result = ws
		.apply_json(
			&serde_json::json!({ "path": "legacy.txt", "edits": [
			{ "op": "update", "diff": "@@\n-alpha\n+beta" },
			{ "op": "create", "diff": "+new" }
		] }),
			&writer,
		)
		.await;
	assert_eq!(std::fs::read(&path).unwrap(), original);
	let err = result.expect_err("initial update needs the original text");
	assert!(err.is_invalid_utf8(), "{err}");
	assert!(writer.requests.lock().is_empty());
}

#[tokio::test]
async fn update_after_delete_rejects_missing_file_without_writing() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("legacy.txt");
	let original = b"name=caf\xe9\n";
	std::fs::write(&path, original).unwrap();
	let writer = DiskWriter::default();
	let result = ws
		.apply_json(
			&serde_json::json!({ "path": "legacy.txt", "edits": [
			{ "op": "delete" },
			{ "op": "update", "diff": "@@\n-alpha\n+beta" }
		] }),
			&writer,
		)
		.await;
	assert_eq!(std::fs::read(&path).unwrap(), original);
	let err = result.expect_err("update cannot resurrect a deleted file");
	assert!(err.to_string().contains("File not found"), "{err}");
	assert!(writer.requests.lock().is_empty());
}

#[tokio::test]
async fn hashline_streaming_rem_previews_invalid_utf8_deletion_before_later_sections() {
	let mut ws = Workspace::new(EditMode::Hashline);
	ws.config.raw_input = true;
	let path = ws.cwd().join("legacy.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	ws.write("next.txt", "next\n");
	let tag = ws.snapshot("next.txt", "next\n", None);
	let mut session = ws.session();
	session.push(&format!("[legacy.txt#FFFF]\nREM\n[next.txt#{tag}]\nREM\n"));
	let streaming = session.preview();
	assert!(streaming.streaming);
	let deletion = streaming
		.files
		.iter()
		.find(|file| file.display == "legacy.txt")
		.expect("completed deletion section must remain visible while streaming");
	assert!(deletion.error.is_none(), "{streaming:?}");
	assert_eq!(deletion.op, Some(FileOp::Delete));
	assert!(path.exists(), "preview must not delete the file");

	session.finish();
	let final_preview = session.preview();
	let deletion = final_preview
		.files
		.iter()
		.find(|file| file.display == "legacy.txt")
		.expect("final preview must retain the deletion");
	assert!(deletion.error.is_none(), "{final_preview:?}");
	assert_eq!(deletion.op, Some(FileOp::Delete));
	let writer = DiskWriter::default();
	session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("apply deletions");
	assert!(!path.exists());
	assert!(!ws.cwd().join("next.txt").exists());
	assert_eq!(writer.requests.lock().len(), 2);
}

#[tokio::test]
async fn hashline_streaming_utf8_bypass_requires_an_allowed_tagged_rem() {
	for (header, diff, original, expected_error) in [
		("[legacy.ts#FFFF]", "PUT EOF:\n|new", b"name=caf\xe9\n".as_slice(), "UTF-8"),
		("[legacy.ts]", "REM", b"name=caf\xe9\n".as_slice(), "UTF-8"),
		("[legacy.ts#FFFF]", "REM\n|body", b"name=caf\xe9\n".as_slice(), "UTF-8"),
		("[legacy.ts#FFFF]", "REM", b"// @generated\nname=caf\xe9\n".as_slice(), "auto-generated"),
	] {
		let mut ws = Workspace::new(EditMode::Hashline);
		ws.config.raw_input = true;
		let path = ws.cwd().join("legacy.ts");
		std::fs::write(&path, original).unwrap();
		ws.write("next.txt", "next\n");
		let tag = ws.snapshot("next.txt", "next\n", None);
		let mut session = ws.session();
		session.push(&format!("{header}\n{diff}\n[next.txt#{tag}]\nREM\n"));
		let preview = session.preview();
		let rejected = preview
			.files
			.iter()
			.find(|file| file.display == "legacy.ts")
			.expect("completed invalid section must report its error");
		assert!(
			rejected
				.error
				.as_deref()
				.is_some_and(|error| error.contains(expected_error)),
			"{header} {diff}: {preview:?}"
		);
		assert_eq!(rejected.op, None);
		session.finish();
		let writer = DiskWriter::default();
		session
			.apply(ApplyRequest::default(), &writer)
			.await
			.expect_err("invalid deletion must not apply");
		assert_eq!(std::fs::read(&path).unwrap(), original);
		assert_eq!(ws.read("next.txt").as_deref(), Some("next\n"));
		assert!(writer.requests.lock().is_empty());
	}
}

#[tokio::test]
async fn hashline_rem_removes_invalid_utf8_file() {
	let ws = Workspace::new(EditMode::Hashline);
	let path = ws.cwd().join("legacy.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	let outcome = ws
		.apply_json(&serde_json::json!({ "input": "[legacy.txt#FFFF]\nREM" }), &writer)
		.await
		.expect("REM needs existence, not text");
	assert!(!path.exists(), "invalid UTF-8 file must be deletable");
	assert!(outcome.text.contains("Deleted legacy.txt"), "{}", outcome.text);
}

#[tokio::test]
async fn hashline_rem_streaming_preview_does_not_error_on_invalid_utf8() {
	use omp_edit::{
		EditStore, PathPolicy,
		session::{Session, SessionConfig},
	};

	let dir = tempfile::tempdir().expect("tempdir");
	let cwd = dir.path().canonicalize().expect("canonical tempdir");
	std::fs::write(cwd.join("legacy.txt"), b"name=caf\xe9\n").unwrap();
	let config = SessionConfig {
		mode:               EditMode::Hashline,
		policy:             PathPolicy {
			cwd:                  cwd.clone(),
			home_dir:             cwd,
			url_schemes:          Vec::new(),
			url_alias_schemes:    Vec::new(),
			plan_writable_roots:  Vec::new(),
			plan_active:          false,
			block_auto_generated: true,
		},
		allow_fuzzy:        true,
		fuzzy_threshold:    0.95,
		enforce_seen_lines: false,
		max_columns:        512,
		raw_input:          false,
	};
	let mut session = Session::new(config, EditStore::new());
	// Completed REM section followed by an incomplete trailing section.
	session.set_args_json(
		&serde_json::json!({ "input": "[legacy.txt#FFFF]\nREM\n[other.txt#FFFF]\nPUT " }).to_string(),
	);
	let batch = session.preview();
	assert!(batch.streaming);
	let completed = batch
		.files
		.iter()
		.find(|file| file.display == "legacy.txt")
		.expect("completed REM section previews");
	assert_eq!(completed.error, None, "{completed:?}");
}

#[tokio::test]
async fn create_over_generated_file_still_rejected() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("gen.ts");
	std::fs::write(&path, "// @generated\nold\n").unwrap();
	let writer = DiskWriter::default();
	let err = ws
		.apply_json(
			&serde_json::json!({ "path": "gen.ts", "edits": [{ "op": "create", "diff": "new\n" }] }),
			&writer,
		)
		.await
		.expect_err("generated-file guard must survive the existence-only path");
	assert!(err.to_string().contains("auto-generated"), "{err}");
	assert_eq!(writer.requests.lock().len(), 0);
}

#[tokio::test]
async fn delete_create_update_replaces_invalid_utf8_file() {
	let ws = Workspace::new(EditMode::Patch);
	let path = ws.cwd().join("a.txt");
	std::fs::write(&path, b"name=caf\xe9\n").unwrap();
	let writer = DiskWriter::default();
	ws.apply_json(
		&serde_json::json!({ "path": "a.txt", "edits": [
			{ "op": "delete" },
			{ "op": "create", "diff": "fresh\n" },
			{ "op": "update", "diff": "@@\n-fresh\n+final" },
		] }),
		&writer,
	)
	.await
	.expect("update after create operates on the new content, not the undecodable bytes");
	assert_eq!(std::fs::read(&path).unwrap(), b"final\n");
}

#[tokio::test]
async fn writer_failure_is_surfaced_verbatim() {
	let ws = Workspace::new(EditMode::Replace);
	ws.write("a.txt", "one\n");
	let writer = DiskWriter { fail_at: Some((0, "disk full".into())), ..Default::default() };
	let err = ws
		.apply_json(
			&serde_json::json!({ "path": "a.txt", "old_string": "one", "new_string": "two" }),
			&writer,
		)
		.await
		.expect_err("writer failure");
	assert_eq!(err.to_string(), "disk full");
}

#[tokio::test]
async fn plan_mode_rejects_working_tree_writes_before_writing() {
	let mut ws = Workspace::new(EditMode::Replace);
	ws.config.policy.plan_active = true;
	ws.write("a.txt", "one\n");
	let writer = DiskWriter::default();
	let err = ws
		.apply_json(
			&serde_json::json!({ "path": "a.txt", "old_string": "one", "new_string": "two" }),
			&writer,
		)
		.await
		.expect_err("plan mode");
	assert_eq!(
		err.to_string(),
		"Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file \
		 instead."
	);
	assert_eq!(writer.requests.lock().len(), 0);
}

#[tokio::test]
async fn internal_url_targets_wait_for_host_answers() {
	let mut ws = Workspace::new(EditMode::Replace);
	ws.config.policy.url_schemes = vec!["local".into()];
	ws.config.policy.plan_active = true;
	let sandbox = tempfile::tempdir().expect("sandbox");
	let backing = sandbox.path().join("plan.md");
	std::fs::write(&backing, "one\n").unwrap();
	let args = r#"{"old_string":"one","new_string":"two","path":"local://plan.md"}"#;
	let writer = DiskWriter::default();

	let mut unprovided = ws.session();
	unprovided.set_args_json(args);
	unprovided.finish();
	let err = unprovided
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect_err("unprovided URL");
	assert!(matches!(&err, EditError::UnresolvedUrl(url) if url == "local://plan.md"), "{err}");
	assert_eq!(writer.requests.lock().len(), 0, "no write before the URL resolves");
	assert_eq!(unprovided.take_unresolved(), ["local://plan.md"]);

	let mut session = ws.session();
	let split = args.len() - r#"an.md"}"#.len();
	session.push(&args[..split]);
	session.preview();
	assert_eq!(
		session.take_unresolved(),
		["local://pl"],
		"streaming passes report their current URL"
	);
	session.push(&args[split..]);
	let batch = session.preview();
	assert_eq!(
		batch.files[0].error.as_deref(),
		Some("Internal URL not resolved yet: local://plan.md")
	);
	assert_eq!(session.take_unresolved(), ["local://plan.md"]);
	assert!(!session.preview_pending());

	session.provide("local://plan.md".into(), UrlAnswer::Backed {
		path:          backing.clone(),
		plan_writable: true,
	});
	assert!(session.preview_pending());
	let batch = session.preview();
	assert_eq!(batch.files[0].display, "local://plan.md");
	assert!(
		batch.files[0]
			.diff
			.as_deref()
			.is_some_and(|diff| diff.contains("+1|two")),
		"{batch:?}"
	);

	session.finish();
	assert_eq!(session.begin_apply_url_targets(), ["local://plan.md"]);
	session.provide("local://plan.md".into(), UrlAnswer::Backed {
		path:          backing.clone(),
		plan_writable: true,
	});
	session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("plan-writable URL applies in plan mode");
	assert_eq!(writer.requests.lock()[0].absolute, backing);
	assert_eq!(std::fs::read_to_string(&backing).unwrap(), "two\n");
}

#[tokio::test]
async fn apply_never_reuses_url_answers_given_to_previews() {
	let mut ws = Workspace::new(EditMode::Replace);
	ws.config.policy.url_schemes = vec!["local".into()];
	ws.config.policy.url_alias_schemes = vec!["local".into()];
	let sandbox = tempfile::tempdir().expect("sandbox");
	let stale = sandbox.path().join("stale.md");
	let fresh = sandbox.path().join("fresh.md");
	std::fs::write(&stale, "one\n").unwrap();
	std::fs::write(&fresh, "one\n").unwrap();
	let answer = |absolute: &std::path::Path| UrlAnswer::Backed {
		path:          absolute.to_owned(),
		plan_writable: false,
	};
	let writer = DiskWriter::default();

	let mut session = ws.session();
	session.set_args_json(r#"{"old_string":"one","new_string":"two","path":"local:/plan.md"}"#);
	session.finish();
	session.preview();
	assert_eq!(session.take_unresolved(), ["local://plan.md"]);
	session.provide("local://plan.md".into(), answer(&stale));
	assert!(session.preview().files[0].diff.is_some(), "the preview uses its answer");

	let err = session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect_err("the preview answer is not reused");
	assert!(matches!(&err, EditError::UnresolvedUrl(url) if url == "local://plan.md"), "{err}");
	assert_eq!(session.take_unresolved(), ["local://plan.md"]);
	session.provide("local://plan.md".into(), answer(&fresh));
	session
		.apply(ApplyRequest::default(), &writer)
		.await
		.expect("apply uses its own answer");
	assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "two\n");
	assert_eq!(std::fs::read_to_string(&stale).unwrap(), "one\n");
}

#[tokio::test]
async fn begin_apply_url_targets_cover_every_url_before_the_first_stage() {
	enum Payload {
		Raw(&'static str),
		Json(serde_json::Value),
	}
	// Every mode that can move a file: section paths plus move destinations.
	let rows = [
		(
			EditMode::ApplyPatch,
			Payload::Raw(
				"*** Begin Patch\n*** Update File: local://a.md\n@@\n-one\n+two\n*** Update File: \
				 local:/b.md\n*** Move to: local://c.md\n@@\n-one\n+two\n*** Update File: \
				 plain.txt\n@@\n-one\n+two\n*** End Patch\n",
			),
			&["local://a.md", "local://b.md", "local://c.md"][..],
		),
		(
			EditMode::Hashline,
			Payload::Json(serde_json::json!({ "input": "[local:/b.md#TAG]\nMV local://c.md" })),
			&["local://b.md", "local://c.md"][..],
		),
		(
			EditMode::Patch,
			Payload::Json(serde_json::json!({ "path": "local:/b.md", "edits": [
				{ "op": "update", "rename": "local://c.md", "diff": "@@\n-one\n+two" },
			] })),
			&["local://b.md", "local://c.md"][..],
		),
	];
	for (mode, payload, expected) in rows {
		let mut ws = Workspace::new(mode);
		ws.config.policy.url_schemes = vec!["local".into()];
		ws.config.policy.url_alias_schemes = vec!["local".into()];
		ws.write("plain.txt", "one\n");
		let sandbox = tempfile::tempdir().expect("sandbox");
		for name in ["a.md", "b.md"] {
			std::fs::write(sandbox.path().join(name), "one\n").unwrap();
		}
		let tag = ws.snapshot(sandbox.path().join("b.md").to_str().unwrap(), "one\n", None);
		let mut session = match payload {
			Payload::Raw(input) => {
				ws.config.raw_input = true;
				let mut session = ws.session();
				session.push(input);
				session
			},
			Payload::Json(args) => {
				let mut session = ws.session();
				session.set_args_json(&args.to_string().replace("#TAG]", &format!("#{tag}]")));
				session
			},
		};
		session.finish();
		let targets = session.begin_apply_url_targets();
		assert_eq!(targets, expected, "{mode:?}");
		for url in targets {
			let name = url.trim_start_matches("local://");
			session.provide(url.clone(), UrlAnswer::Backed {
				path:          sandbox.path().join(name),
				plan_writable: false,
			});
		}
		let writer = DiskWriter::default();
		session
			.apply(ApplyRequest::default(), &writer)
			.await
			.unwrap_or_else(|err| panic!("{mode:?}: every URL answered up front, yet {err}"));
		assert!(sandbox.path().join("c.md").exists(), "{mode:?}");
		assert!(!sandbox.path().join("b.md").exists(), "{mode:?}");
	}
}

#[tokio::test]
async fn result_text_uses_compact_preview_and_header() {
	let ws = Workspace::new(EditMode::Replace);
	ws.write("a.txt", "one\ntwo\nthree\n");
	let writer = DiskWriter::default();
	let outcome = ws
		.apply_json(
			&serde_json::json!({ "path": "a.txt", "old_string": "two", "new_string": "TWO" }),
			&writer,
		)
		.await
		.expect("apply");
	assert!(outcome.text.starts_with("[a.txt]\n"), "{}", outcome.text);
	assert!(outcome.text.contains("\n2:TWO\n"), "{}", outcome.text);
	assert!(outcome.files[0].diff.contains("+2|TWO"), "{}", outcome.files[0].diff);
	assert_eq!(outcome.files[0].old_text.as_deref(), Some("one\ntwo\nthree\n"));
	assert_eq!(outcome.files[0].new_text.as_deref(), Some("one\nTWO\nthree\n"));
}
