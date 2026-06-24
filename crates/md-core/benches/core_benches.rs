//! Hot-path benchmarks: parse, content-hash, frontmatter splice, section patch.
//!
//! Run with `CRITERION_HOME=$PWD/.local/bench cargo bench -p md-core` so reports
//! land in the git-ignored `.local/` directory.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use md_core::{Document, Edit, Operation, Scope, frontmatter, patch};

fn sample_note(sections: usize) -> String {
    let mut s = String::from("---\ntitle: Sample\ntags:\n  - a\n  - b\n---\n");
    for i in 0..sections {
        s.push_str(&format!(
            "# Section {i}\nSome body text for section {i}.\n\n## Sub {i}\nNested body.\n\n"
        ));
    }
    s
}

fn bench_parse(c: &mut Criterion) {
    let note = sample_note(50);
    c.bench_function("document_parse_50_sections", |b| {
        b.iter(|| Document::parse(black_box(&note)))
    });
}

fn bench_content_hash(c: &mut Criterion) {
    let note = sample_note(50);
    let doc = Document::parse(&note);
    let idx = doc.resolve_heading(&["Section 25".into()], None).unwrap();
    c.bench_function("content_hash_section", |b| {
        b.iter(|| doc.content_hash(black_box(&note), Some(idx), Scope::Section));
    });
}

fn bench_frontmatter_set(c: &mut Criterion) {
    let note = sample_note(10);
    c.bench_function("frontmatter_set_property", |b| {
        b.iter(|| {
            frontmatter::set_property(black_box(&note), "status", &serde_json::json!("draft"))
                .unwrap()
        });
    });
}

fn bench_patch(c: &mut Criterion) {
    let note = sample_note(50);
    let edit = Edit {
        heading_path: vec!["Section 25".into()],
        operation: Some(Operation::Replace),
        scope: Scope::Body,
        content: Some("Replaced body.".into()),
        ..Edit::default()
    };
    c.bench_function("patch_replace_body", |b| {
        b.iter(|| patch::patch_sections(black_box(&note), std::slice::from_ref(&edit)).unwrap());
    });
}

criterion_group!(
    benches,
    bench_parse,
    bench_content_hash,
    bench_frontmatter_set,
    bench_patch
);
criterion_main!(benches);
