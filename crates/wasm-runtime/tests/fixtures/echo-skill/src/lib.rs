use workforce_skill_sdk::prelude::*;

workforce_skill_sdk::init!();

// Tool 1: Echo — returns the input message back
workforce_skill_sdk::tool!(echo, |input: ToolInput| {
    let message = input.get_str("message").unwrap_or("(no message)");
    log::info(&format!("echo called with: {}", message));
    ToolOutput::success(json!({
        "echoed": message,
        "tool_name": input.tool_name,
        "agent_id": input.agent_id,
    }))
});

// Tool 2: Add — adds two numbers
workforce_skill_sdk::tool!(add, |input: ToolInput| {
    let a = match input.get_f64("a") {
        Some(v) => v,
        None => return ToolOutput::error("Missing parameter 'a'"),
    };
    let b = match input.get_f64("b") {
        Some(v) => v,
        None => return ToolOutput::error("Missing parameter 'b'"),
    };
    ToolOutput::success(json!({
        "sum": a + b,
        "a": a,
        "b": b,
    }))
});

// Tool 3: Fail — always returns an error
workforce_skill_sdk::tool!(fail, |_input: ToolInput| {
    ToolOutput::error("This tool always fails on purpose")
});

// Tool 4: Log test — exercises all log levels
workforce_skill_sdk::tool!(log_test, |_input: ToolInput| {
    log::error("test error message");
    log::warn("test warning message");
    log::info("test info message");
    log::debug("test debug message");
    ToolOutput::success(json!({"logged": true}))
});

// Tool 5: Get time — exercises util::current_time and util::generate_uuid
workforce_skill_sdk::tool!(get_time, |_input: ToolInput| {
    let time = util::current_time();
    let uuid = util::generate_uuid();
    ToolOutput::success(json!({
        "time": time,
        "uuid": uuid,
    }))
});

// Tool 6: Vault read — exercises vault::get host function
workforce_skill_sdk::tool!(vault_read, |input: ToolInput| {
    let key = input.get_str("key").unwrap_or("unknown");
    match vault::get(key) {
        Some(value) => ToolOutput::success(json!({
            "key": key,
            "value": value,
            "found": true,
        })),
        None => ToolOutput::success(json!({
            "key": key,
            "found": false,
        })),
    }
});

// Tool 7: Infinite loop — for testing timeout enforcement
#[allow(unreachable_code)]
workforce_skill_sdk::tool!(infinite_loop, |_input: ToolInput| {
    loop {
        // Spin forever — the epoch interrupt should kill this
    }
    ToolOutput::error("unreachable")
});

// Tool 8: Config read — exercises config::get host function
workforce_skill_sdk::tool!(config_read, |input: ToolInput| {
    let key = input.get_str("key").unwrap_or("unknown");
    match config::get(key) {
        Some(value) => ToolOutput::success(json!({
            "key": key,
            "value": value,
            "found": true,
        })),
        None => ToolOutput::success(json!({
            "key": key,
            "found": false,
        })),
    }
});

// Tool 9: invoke_many passthrough — exercises wf_tool_invoke_many
workforce_skill_sdk::tool!(invoke_many_test, |input: ToolInput| {
    let calls: Vec<(String, Value)> = input
        .get("calls")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e.get("tool")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        e.get("args").cloned().unwrap_or_else(|| json!({})),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let borrowed: Vec<(&str, Value)> = calls
        .iter()
        .map(|(name, args)| (name.as_str(), args.clone()))
        .collect();
    match tools::invoke_many(&borrowed) {
        Ok(results) => ToolOutput::success(json!({ "results": results })),
        Err(e) => ToolOutput::success(json!({ "batch_error": e })),
    }
});

// Tool 10: fetch burst — exercises the wf_http_fetch per-invocation budget
workforce_skill_sdk::tool!(fetch_burst, |input: ToolInput| {
    let count = input.get_i64("count").unwrap_or(1);
    let url = input
        .get_str("url")
        .unwrap_or("https://blocked.example.com/");
    let completed = (0..count)
        .filter(|_| http::fetch("GET", url, &[], None).is_some())
        .count();
    ToolOutput::success(json!({ "attempted": count, "completed": completed }))
});
