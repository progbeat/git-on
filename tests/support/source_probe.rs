//! Private acceptance helper: exercises the same loader without mounting FUSE.
use anyhow::{Context, Result};
use git_on::snapshot::Snapshot;
use std::{
    env,
    io::{self, BufRead, Write},
    path::Path,
};
fn report(snapshot: &Snapshot) -> Result<()> {
    let repo = git2::Repository::open(&snapshot.git_dir)?;
    let odb = repo.odb()?;
    let mut objects = Vec::new();
    odb.foreach(|oid| {
        objects.push(oid.to_string());
        true
    })?;
    objects.sort();
    println!(
        "{}",
        serde_json::json!({"tree": snapshot.tree_oid.to_string(), "objects": objects, "metrics": snapshot.metrics()})
    );
    io::stdout().flush()?;
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let source = args.first().context("source required")?;
    let revision = args.get(1).context("revision required")?;
    let run = tempfile::tempdir()?;
    let snapshot = Snapshot::load_source_with_pin_record(
        &env::current_dir()?,
        (source != "local").then_some(source.as_os_str()),
        revision,
        args.iter().any(|x| x == "--verify"),
        run.path(),
        "probe",
        |_, _, _| Ok(()),
    )?;
    report(&snapshot)?;
    let mut opened = None;
    for operation in io::stdin().lock().lines() {
        let operation = operation?;
        if operation == "exit" {
            break;
        }
        if let Some(arguments) = operation.strip_prefix("range ") {
            let mut parts = arguments.splitn(3, ' ');
            let offset = parts.next().context("offset")?.parse()?;
            let size = parts.next().context("size")?.parse()?;
            let path = parts.next().context("path")?;
            let entry = snapshot
                .entry(Path::new(path))?
                .context("missing probe path")?;
            if opened.as_ref().is_none_or(|(oid, _)| *oid != entry.oid) {
                opened = Some((entry.oid, snapshot.open(entry.oid)?));
            }
            let data = opened.as_ref().unwrap().1.read(offset, size)?;
            println!("{}", serde_json::json!({"bytes": data}));
        } else if let Some(path) = operation.strip_prefix("stat ") {
            let entry = snapshot
                .entry(Path::new(path))?
                .context("missing probe path")?;
            println!("{}", serde_json::json!({"size": entry.size}));
        } else if operation == "release" {
            opened = None;
            println!("{}", serde_json::json!({"released": true}));
        } else if let Some(path) = operation.strip_prefix("read ") {
            let entry = snapshot
                .entry(Path::new(path))?
                .context("missing probe path")?;
            let data = snapshot.read(entry.oid, 0, 1024 * 1024)?;
            println!(
                "{}",
                serde_json::json!({"data": String::from_utf8_lossy(&data)})
            );
        } else if let Some(path) = operation.strip_prefix("list ") {
            let entries = snapshot.children(Path::new(path))?;
            println!(
                "{}",
                serde_json::json!({"names": entries.iter().map(|x| x.0.to_string_lossy()).collect::<Vec<_>>()})
            );
        } else if operation != "report" {
            anyhow::bail!("unknown probe operation");
        }
        report(&snapshot)?;
    }
    drop(snapshot);
    Ok(())
}
