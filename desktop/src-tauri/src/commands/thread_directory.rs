use tauri::State;

use crate::{app_state::AppState, relay::query_relay};

const KIND_THREAD_DIRECTORY_ITEM: u32 = 39007;
const KIND_THREAD_DIRECTORY_BOUNDS: u32 = 39008;

fn build_thread_directory_filter(
    channel_id: &str,
    directory_state: &str,
    cursor: Option<&str>,
    limit_rows: u32,
) -> Result<serde_json::Value, String> {
    if !matches!(directory_state, "active" | "archived") {
        return Err("directory state must be active or archived".to_string());
    }

    Ok(serde_json::json!({
        "kinds": [KIND_THREAD_DIRECTORY_ITEM, KIND_THREAD_DIRECTORY_BOUNDS],
        "#h": [channel_id],
        "limit": limit_rows.min(100),
        "thread_index": true,
        "directory_state": directory_state,
        "directory_cursor": cursor,
    }))
}

/// Fetch one channel-scoped thread-directory page through the authenticated
/// relay query bridge. The relay synthesizes the returned directory overlays.
#[tauri::command]
pub async fn get_thread_directory(
    channel_id: String,
    directory_state: String,
    cursor: Option<String>,
    limit_rows: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    let filter = build_thread_directory_filter(
        &channel_id,
        &directory_state,
        cursor.as_deref(),
        limit_rows.unwrap_or(25),
    )?;

    Ok(query_relay(&state, &[filter])
        .await?
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .collect())
}
