//! A status page's logo and favicon: upload and remove.

use topcoat::{
    Result,
    context::Cx,
    router::{
        content::multipart::Multipart,
        error::{SeeOther, not_found, see_other},
        path_param, query_params, route,
    },
    view::{View, component, view},
};
use uptime_store::{PageImage, StoreError};

use super::Id;
use crate::{
    auth::{app, require_admin},
    media::{self, MediaError},
};

path_param!(slot);

#[query_params]
pub(crate) struct ImageQuery {
    image_error: Option<String>,
}

/// The upload problem to show on the editor, from `?image_error=`.
pub(crate) fn image_error(cx: &Cx) -> Option<&'static str> {
    let code = query_params::<ImageQuery>(cx).ok()?.image_error.clone()?;
    Some(match code.as_str() {
        "too_large" => "That image is larger than 512 KB.",
        "unsupported" => "Upload a PNG, JPEG, WebP, GIF, SVG or ICO image.",
        "missing" => "Choose a file first.",
        "disabled" => "Image uploads are not configured on this server (storage.path).",
        _ => return None,
    })
}

fn page_id(cx: &Cx) -> Result<i64> {
    Ok(*path_param::<Id>(cx)?)
}

fn slot(cx: &Cx) -> Result<PageImage> {
    match path_param::<Slot>(cx) {
        "logo" => Ok(PageImage::Logo),
        "favicon" => Ok(PageImage::Favicon),
        _ => Err(not_found().into()),
    }
}

fn back(id: i64, error: Option<&str>) -> SeeOther {
    match error {
        Some(code) => see_other(format!("/admin/pages/{id}?image_error={code}#images")),
        None => see_other(format!("/admin/pages/{id}#images")),
    }
}

/// Deletes `file` unless another page still uses it.
async fn forget(cx: &Cx, file: Option<String>, keep: Option<&str>) -> Result<()> {
    let state = app(cx);
    if let (Some(file), Some(media)) = (file, &state.media)
        && Some(file.as_str()) != keep
        && !state.store.image_in_use(&file).await?
    {
        media.delete(&file).await;
    }
    Ok(())
}

#[route(POST "/admin/pages/{id}/images/{slot}")]
pub(crate) async fn upload_image(cx: &Cx, mut form: Multipart) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = page_id(cx)?;
    let slot = slot(cx)?;
    let state = app(cx);
    let Some(media) = &state.media else {
        return Ok(back(id, Some("disabled")));
    };
    let mut upload = None;
    while let Some(field) = form.next_field().await? {
        if field.name() == Some("image") {
            upload = Some(field.bytes().await?);
            break;
        }
    }
    let Some(bytes) = upload.filter(|bytes| !bytes.is_empty()) else {
        return Ok(back(id, Some("missing")));
    };
    let file = match media.save_image(&bytes).await {
        Ok(file) => file,
        Err(MediaError::TooLarge) => return Ok(back(id, Some("too_large"))),
        Err(MediaError::Unsupported) => return Ok(back(id, Some("unsupported"))),
        Err(error @ MediaError::Io(_)) => return Err(std::io::Error::other(error).into()),
    };
    let previous = match state.store.set_page_image(id, slot, Some(&file)).await {
        Ok(previous) => previous,
        Err(StoreError::PageNotFound(_)) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    };
    forget(cx, previous, Some(&file)).await?;
    state.pages.clear();
    tracing::info!(page = id, ?slot, file, by = %admin.login, "page image uploaded");
    Ok(back(id, None))
}

#[route(POST "/admin/pages/{id}/images/{slot}/delete")]
pub(crate) async fn remove_image(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = page_id(cx)?;
    let slot = slot(cx)?;
    let state = app(cx);
    let previous = match state.store.set_page_image(id, slot, None).await {
        Ok(previous) => previous,
        Err(StoreError::PageNotFound(_)) => return Err(not_found().into()),
        Err(error) => return Err(error.into()),
    };
    forget(cx, previous, None).await?;
    state.pages.clear();
    tracing::info!(page = id, ?slot, by = %admin.login, "page image removed");
    Ok(back(id, None))
}

/// The editor's "Logo and favicon" card.
#[component]
pub(crate) async fn images_section(
    page_id: i64,
    logo: Option<String>,
    favicon: Option<String>,
    enabled: bool,
    #[default] error: Option<&'static str>,
) -> Result<impl View> {
    let slots = [
        ("logo", "Logo", "Shown in the page header.", logo),
        (
            "favicon",
            "Favicon",
            "Shown in browser tabs. Without one, the logo is used.",
            favicon,
        ),
    ];
    Ok(view! {
        <section id="images" class="card stack">
            <div>
                <h2>"Logo and favicon"</h2>
                <p class="muted small">"PNG, JPEG, WebP, GIF, SVG or ICO, up to 512 KB."</p>
            </div>
            if let Some(error) = error {
                <p class="flash flash-error" role="alert">(error)</p>
            }
            if !enabled {
                <p class="muted small">"Uploads need a storage directory (UPTIMESTATUS_STORAGE__PATH)."</p>
            } else {
                <div class="cols">
                    for (slot, label, hint, current) in &slots {
                        <div class="image-slot">
                            <h3>(*label)</h3>
                            <div class="image-preview grid-bg">
                                if let Some(file) = current {
                                    <img src=(media::url(file)) alt=(format!("Current {}", label.to_lowercase()))>
                                } else {
                                    <span class="muted small">"None yet"</span>
                                }
                            </div>
                            <p class="muted small">(*hint)</p>
                            <form class="row" method="post" enctype="multipart/form-data"
                                action=(format!("/admin/pages/{page_id}/images/{slot}"))>
                                <input type="file" name="image" required=""
                                    accept="image/png,image/jpeg,image/webp,image/gif,image/svg+xml,image/x-icon,.ico">
                                <button class="btn btn-primary" type="submit">"Upload"</button>
                            </form>
                            if current.is_some() {
                                <form method="post" action=(format!("/admin/pages/{page_id}/images/{slot}/delete"))>
                                    <button class="btn btn-link" type="submit">(format!("Remove {}", label.to_lowercase()))</button>
                                </form>
                            }
                        </div>
                    }
                </div>
            }
        </section>
    })
}
