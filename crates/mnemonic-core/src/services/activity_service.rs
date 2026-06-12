use crate::models::ActivityItem;
use crate::repositories::activity_repository;
use crate::state::AppState;

pub fn list_activity(state: &AppState) -> Result<Vec<ActivityItem>, String> {
    activity_repository::list(&state.db)
}
