use super::*;

#[tokio::test]
async fn non_debug_commands_are_left_to_the_caller() {
    let commands = DebugCommands::new(Arc::new(Indexer::new()));
    let outcome = commands.execute("kmp-lsp/reindex", &[]).await;
    assert!(matches!(outcome, CommandOutcome::NotDebugCommand));
}

#[tokio::test]
async fn resolve_classpath_answers_jdtls_shape() {
    let commands = DebugCommands::new(Arc::new(Indexer::new()));
    let outcome = commands
        .execute(
            RESOLVE_CLASSPATH,
            &[Value::from("my.module/com.example.MainKt")],
        )
        .await;
    let CommandOutcome::Answered(value) = outcome else {
        panic!("resolveClasspath should answer");
    };
    // `[modulepaths, classpaths]`, both arrays, even without a workspace.
    assert_eq!(value, serde_json::json!([[], []]));
}

#[tokio::test]
async fn resolve_classpath_without_main_class_fails() {
    let commands = DebugCommands::new(Arc::new(Indexer::new()));
    let outcome = commands.execute(RESOLVE_CLASSPATH, &[]).await;
    assert!(matches!(outcome, CommandOutcome::Failed(_)));
}

#[test]
fn module_prefix_is_stripped_from_main_class() {
    assert_eq!(
        strip_module_prefix("my.module/com.example.Main"),
        "com.example.Main"
    );
    assert_eq!(strip_module_prefix("com.example.Main"), "com.example.Main");
}

#[test]
fn every_offered_command_is_answered() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let commands = DebugCommands::new(Arc::new(Indexer::new()));
    for command in COMMANDS {
        // Starting the debug server needs the jar and a JVM; covered elsewhere.
        if *command == START_DEBUG_SESSION {
            continue;
        }
        let outcome =
            runtime.block_on(commands.execute(command, &[Value::from("com.example.Main")]));
        assert!(
            !matches!(outcome, CommandOutcome::NotDebugCommand),
            "{command} is advertised but not handled"
        );
    }
}
