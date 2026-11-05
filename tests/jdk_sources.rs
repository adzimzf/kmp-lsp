//! The project's JDK sources, over the real `kmp-lsp --stdio` server.
//!
//! A fake JDK (`bin/java` + `lib/src.zip`) is the server's `$JAVA_HOME`.
//! The workspace calls `ArrayList.add` and `StringBuilder.append` next to a
//! decoy `Bag` declaring the same member names: before the JDK was indexed,
//! go-to-definition landed on `Bag` and find-references kept every `add`.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

const USE_KT: &str = "src/main/kotlin/app/Use.kt";
const BAG_KT: &str = "src/main/kotlin/app/Bag.kt";
const USE_SOURCE: &str = "package app\n\
\n\
import java.util.ArrayList\n\
\n\
fun fill(items: ArrayList<String>, builder: StringBuilder) {\n\
\x20   items.add(\"first\")\n\
\x20   val more = ArrayList<String>()\n\
\x20   more.add(\"second\")\n\
\x20   builder.append(\"text\")\n\
}\n";
const BAG_SOURCE: &str = "package app\n\
\n\
class Bag {\n\
\x20   fun add(item: String) {}\n\
\x20   fun append(text: String) {}\n\
}\n\
\n\
fun decoys() {\n\
\x20   Bag().add(\"decoy\")\n\
\x20   Bag().append(\"decoy\")\n\
}\n";

/// The JDK's own sources mount in the background after indexing.
const JDK_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A JDK 9+ layout: `bin/java` and a module-prefixed `lib/src.zip`.
fn write_fake_jdk(home: &Path) {
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/java"), "").unwrap();
    std::fs::create_dir_all(home.join("lib")).unwrap();
    let sources = [
        (
            "java.base/java/util/ArrayList.java",
            "package java.util;\n\npublic class ArrayList<E> {\n    public ArrayList() {}\n    public boolean add(E e) { return true; }\n}\n",
        ),
        (
            "java.base/java/lang/StringBuilder.java",
            "package java.lang;\n\npublic final class StringBuilder {\n    public StringBuilder() {}\n    public StringBuilder append(String text) { return this; }\n}\n",
        ),
        (
            "java.base/java/util/Collections.java",
            "package java.util;\n\npublic class Collections {\n    static void fill(ArrayList<String> list) { list.add(\"jdk internal\"); }\n}\n",
        ),
    ];
    let mut writer = zip::ZipWriter::new(std::fs::File::create(home.join("lib/src.zip")).unwrap());
    for (path, text) in sources {
        writer
            .start_file(path, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
}

struct JdkWorkspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    client: LspClient,
}

impl JdkWorkspace {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical_root(dir.path());
        let jdk = base.join("jdk");
        write_fake_jdk(&jdk);
        let root = base.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "workspace.json", r#"{"sourcePaths":[]}"#);
        write(&root, USE_KT, USE_SOURCE);
        write(&root, BAG_KT, BAG_SOURCE);
        let environment = ServerEnvironment {
            gradle_user_home: None,
            variables: vec![
                ("JAVA_HOME".to_owned(), jdk),
                ("KMP_LSP_JDK_SOURCES".to_owned(), PathBuf::from("on")),
                ("XDG_CACHE_HOME".to_owned(), base.join("xdg-cache")),
            ],
        };
        let mut client = LspClient::spawn_with(&root, &environment);
        client.initialize(&root);
        client.wait_for_indexing();
        for (path, text) in [(USE_KT, USE_SOURCE), (BAG_KT, BAG_SOURCE)] {
            client.open_file(&file_uri(&root, path), "kotlin", text);
        }
        JdkWorkspace {
            _dir: dir,
            root,
            client,
        }
    }

    /// 0-based position of `token` on the line of `Use.kt` containing `needle`.
    fn spot(needle: &str, token: &str) -> (u64, u64) {
        let (line_index, line) = USE_SOURCE
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap();
        let column = line.find(needle).unwrap() + needle.find(token).unwrap();
        (line_index as u64, column as u64)
    }

    fn request(&mut self, method: &str, at: (u64, u64)) -> Vec<(String, u64)> {
        let mut params = json!({
            "textDocument": {"uri": file_uri(&self.root, USE_KT)},
            "position": {"line": at.0, "character": at.1},
        });
        if method == "textDocument/references" {
            params["context"] = json!({"includeDeclaration": false});
        }
        locations(&self.client.request(method, params)["result"])
    }

    /// Go-to-definition, re-asked until it reaches a file named `file_name`
    /// (the JDK mounts asynchronously after indexing).
    fn definition_once_mounted(&mut self, at: (u64, u64), file_name: &str) -> Vec<(String, u64)> {
        let deadline = Instant::now() + JDK_READY_TIMEOUT;
        loop {
            let found = self.request("textDocument/definition", at);
            let reached = found.iter().any(|(uri, _)| uri.ends_with(file_name));
            if reached || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn file_names(found: &[(String, u64)]) -> Vec<String> {
    found
        .iter()
        .map(|(uri, line)| format!("{}:{}", uri.rsplit('/').next().unwrap_or(uri), line + 1))
        .collect()
}

#[test]
fn jdk_members_resolve_and_their_references_skip_same_named_decoys() {
    let mut workspace = JdkWorkspace::start();

    let add = JdkWorkspace::spot("items.add", "add");
    let definition = workspace.definition_once_mounted(add, "ArrayList.java");
    assert!(
        !definition.is_empty()
            && definition
                .iter()
                .all(|(uri, _)| uri.ends_with("ArrayList.java")),
        "`items.add` must open the JDK's ArrayList.add, not Bag.add: {:?}",
        file_names(&definition)
    );

    let append = JdkWorkspace::spot("builder.append", "append");
    let definition = workspace.definition_once_mounted(append, "StringBuilder.java");
    assert!(
        !definition.is_empty()
            && definition
                .iter()
                .all(|(uri, _)| uri.ends_with("StringBuilder.java")),
        "`builder.append` (java.lang, no import) must open StringBuilder.append: {:?}",
        file_names(&definition)
    );

    let references = file_names(&workspace.request("textDocument/references", add));
    assert!(
        references.contains(&"Use.kt:6".to_owned()) && references.contains(&"Use.kt:8".to_owned()),
        "both ArrayList.add calls are references: {references:?}"
    );
    assert!(
        !references.contains(&"Bag.kt:9".to_owned()),
        "`Bag().add` is not a call of ArrayList.add: {references:?}"
    );
    assert!(
        !references
            .iter()
            .any(|hit| hit.starts_with("Collections.java")),
        "the JDK's own sources are not searched for usages: {references:?}"
    );
}
