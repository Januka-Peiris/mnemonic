use crate::models::ReviewItem;
use crate::repositories::review_repository;
use crate::state::AppState;

pub fn list_pending_reviews(state: &AppState) -> Result<Vec<ReviewItem>, String> {
    review_repository::list_pending(&state.db)
}
