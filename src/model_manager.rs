use crate::model_config::{
    SavedModelListItem, activate_saved_model, compact_timestamp, delete_saved_model,
    list_saved_models,
};
use crate::provider::RuntimeProvider;
use crate::setup::{
    PickerEditAction, PickerItem, SetupAction, format_model_manager_add_row,
    format_model_manager_saved_row, is_runtime_setup_cancelled, prompt_confirm,
    run_model_manager_picker, run_saved_model_setup_with_back, show_setup_message,
};
use anyhow::Result;

const ADD_ROW_INDEX: usize = 0;

pub(crate) fn run_model_manager() -> Result<Option<RuntimeProvider>> {
    let mut selected_runtime = None;
    loop {
        let saved_models = list_saved_models()?;
        let items = model_manager_picker_items(&saved_models);
        let action = match run_model_manager_picker(
            "Models",
            "Enter marks the selected model as Main. Delete removes a saved model.",
            &items,
        ) {
            Ok(action) => action,
            Err(error) if is_runtime_setup_cancelled(&error) => return Ok(selected_runtime),
            Err(error) => return Err(error),
        };
        match action {
            PickerEditAction::Submit(ADD_ROW_INDEX) => {
                match run_saved_model_setup_with_back(true) {
                    Ok(SetupAction::Submit(runtime)) => selected_runtime = Some(runtime),
                    Ok(SetupAction::Back) => {}
                    Err(error) if is_runtime_setup_cancelled(&error) => {
                        return Ok(selected_runtime);
                    }
                    Err(error) => return Err(error),
                }
            }
            PickerEditAction::Submit(index) => {
                if let Some(item) = saved_models.get(index.saturating_sub(1)) {
                    selected_runtime = Some(activate_saved_model(&item.saved_model.model_id)?);
                }
            }
            PickerEditAction::Delete(ADD_ROW_INDEX) => {
                show_setup_message("Cannot delete", "Select a saved model first.")?;
            }
            PickerEditAction::Delete(index) => {
                if let Some(item) = saved_models.get(index.saturating_sub(1)) {
                    if confirm_delete_saved_model(item)? {
                        let deleted_selected_runtime = selected_runtime
                            .as_ref()
                            .and_then(|runtime| runtime.model_id.as_deref())
                            .is_some_and(|model_id| model_id == item.saved_model.model_id)
                            || item.active;
                        let replacement_runtime = delete_saved_model(&item.saved_model.model_id)?;
                        if deleted_selected_runtime {
                            selected_runtime = replacement_runtime;
                        }
                    }
                }
            }
        }
    }
}

fn confirm_delete_saved_model(item: &SavedModelListItem) -> Result<bool> {
    let lines = vec![
        format!(
            "{} / {}",
            item.saved_model.provider.as_str(),
            item.saved_model.model.as_str()
        ),
        format!("Endpoint: {}", item.endpoint),
        format!("Key: {}", item.key_fingerprint),
        "This removes the saved model and its model-specific credential.".to_string(),
    ];
    Ok(matches!(
        prompt_confirm("Delete model", &lines, true)?,
        SetupAction::Submit(())
    ))
}

fn model_manager_picker_items(saved_models: &[SavedModelListItem]) -> Vec<PickerItem> {
    let mut items = Vec::with_capacity(saved_models.len() + 1);
    items.push(PickerItem {
        title: format_model_manager_add_row(),
        detail: String::new(),
        model_columns: None,
    });
    items.extend(saved_models.iter().map(|item| PickerItem {
        title: format_saved_model_row(item),
        detail: String::new(),
        model_columns: None,
    }));
    items
}

fn format_saved_model_row(item: &SavedModelListItem) -> String {
    let added = compact_timestamp(&item.saved_model.created_at);
    let added = added.get(..16).unwrap_or(&added);
    format_model_manager_saved_row(
        item.active,
        &item.saved_model.provider,
        &item.saved_model.model,
        &item.endpoint,
        &item.key_fingerprint,
        added,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::SavedModel;

    #[test]
    fn formats_saved_model_row_without_name() {
        let item = SavedModelListItem {
            saved_model: SavedModel {
                model_id: "m1".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                api_mode: None,
                created_at: "2026-05-17T16:20:00+00:00".to_string(),
                last_used_at: None,
            },
            active: true,
            endpoint: "api.deepseek.com".to_string(),
            key_fingerprint: "...9f2a".to_string(),
        };
        let row = format_saved_model_row(&item);
        assert!(row.contains("deepseek"));
        assert!(row.contains("deepseek-chat"));
        assert!(row.contains("...9f2a"));
        assert!(!row.contains("m1"));
        assert!(row.contains("*"));
        assert!(!row.starts_with(" 1"));
    }

    #[test]
    fn empty_model_manager_still_shows_add_row() {
        let items = model_manager_picker_items(&[]);
        assert_eq!(items.len(), 1);
        assert!(items[0].title.contains("Add"));
        assert!(items[0].detail.is_empty());
    }
}
