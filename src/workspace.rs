use context::*;
use lsp_types::request::Request;
use lsp_types::*;
use serde::Deserialize;
use serde_json::{self, Value};
use toml;
use types::*;
use util::*;

use lsp_types::notification::{self, Notification};

fn insert_value<'a, 'b, P>(
    target: &'b mut serde_json::map::Map<String, Value>,
    mut path: P,
    local_key: String,
    value: Value,
) -> Result<(), String>
where
    P: Iterator<Item = &'a str>,
    P: 'a,
{
    match path.next() {
        Some(key) => {
            let mut maybe_new_target = target
                .entry(key)
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut();

            if maybe_new_target.is_none() {
                return Err(format!(
                    "Expected path {:?} to be object, found {:?}",
                    key, &maybe_new_target,
                ));
            }

            insert_value(maybe_new_target.unwrap(), path, local_key, value)
        }
        None => match target.insert(local_key, value) {
            Some(old_value) => Err(format!("Replaced old value: {:?}", old_value)),
            None => Ok(()),
        },
    }
}

pub fn did_change_configuration(params: EditorParams, ctx: &mut Context) {
    let default_settings = toml::value::Table::new();

    let raw_settings = params
        .as_table()
        .and_then(|t| t.get("settings"))
        .and_then(|val| val.as_table())
        .unwrap_or_else(|| &default_settings);

    let mut settings = serde_json::Map::new();

    for (raw_key, raw_value) in raw_settings.iter() {
        let mut key_parts = raw_key.split('.');
        let local_key = match key_parts.next_back() {
            Some(name) => name,
            None => {
                warn!("Got a setting with an empty local name: {:?}", raw_key);
                continue;
            }
        };

        let value: Value = match raw_value.clone().try_into() {
            Ok(value) => value,
            Err(e) => {
                warn!("Could not convert setting {:?} to JSON: {}", raw_value, e,);
                continue;
            }
        };

        match insert_value(&mut settings, key_parts, local_key.into(), value) {
            Ok(_) => (),
            Err(e) => {
                warn!("Could not set {:?} to {:?}: {}", raw_key, raw_value, e);
                continue;
            }
        }
    }

    let params = DidChangeConfigurationParams {
        settings: Value::Object(settings),
    };
    ctx.notify(notification::DidChangeConfiguration::METHOD.into(), params);
}

pub fn workspace_symbol(meta: &EditorMeta, params: EditorParams, ctx: &mut Context) {
    let req_params = WorkspaceSymbolParams::deserialize(params.clone());
    if req_params.is_err() {
        error!("Params should follow WorkspaceSymbolParams structure");
        return;
    }
    let req_params = req_params.unwrap();
    let id = ctx.next_request_id();
    ctx.response_waitlist.insert(
        id.clone(),
        (
            meta.clone(),
            request::WorkspaceSymbol::METHOD.into(),
            params,
        ),
    );
    ctx.call(id, request::WorkspaceSymbol::METHOD.into(), req_params);
}

pub fn editor_workspace_symbol(meta: &EditorMeta, result: Value, ctx: &mut Context) {
    let result: Option<Vec<SymbolInformation>> =
        serde_json::from_value(result).expect("Failed to parse workspace symbol response");
    if result.is_none() {
        return;
    }
    let result = result.unwrap();
    let content = format_symbol_information(result, ctx);
    let command = format!(
        "lsp-show-workspace-symbol {} {}",
        editor_quote(&ctx.root_path),
        editor_quote(&content),
    );
    ctx.exec(meta.clone(), command);
}
