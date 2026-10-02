//! Public routes that are not status pages.

use topcoat::{
    Result,
    router::{
        error::{SeeOther, see_other},
        route,
    },
};

/// The app host's root: the admin console (which asks unauthenticated visitors to sign in).
#[route(GET "/")]
pub(crate) async fn root() -> Result<SeeOther> {
    Ok(see_other("/admin"))
}
