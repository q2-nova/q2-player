//! Whole-catalog diagnostic compatibility calls share async transport and the
//! same generation-checked DB owner as the paged product screens.
use super::{Client, LibError, Library, Policy, fail, segment, text};
use std::sync::Arc;

type Rows = Vec<(String, String)>;

impl Library {
    pub fn shelf_items(&self, podcasts: bool, policy: Policy) -> Result<Rows, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        super::super::shelf_browse::wait(async move { lib.shelf_items_async(podcasts, policy).await })
    }

    pub async fn shelf_items_async(self: &Arc<Self>, podcasts: bool, policy: Policy) -> Result<Rows, LibError> {
        self.legacy_shelf_rows(if podcasts { "podcasts" } else { "books" }.into(), policy, move |client| async move {
            let libraries = client.get_async("api/libraries").await.map_err(fail)?;
            let ids = crate::rt::spawn_blocking(move |_| {
                Ok::<_, LibError>(libraries["libraries"].as_array()
                    .ok_or_else(|| fail("Invalid Audiobookshelf libraries"))?.iter()
                    .filter(|library| library["mediaType"].as_str() == Some(if podcasts { "podcast" } else { "book" }))
                    .map(|library| text(&library["id"])).collect::<Vec<_>>())
            }).await.map_err(fail)??;
            let mut items = Vec::new();
            for id in ids {
                for page in 0..1000 {
                    let response = client.get_query_async(&format!("api/libraries/{}/items", segment(&id)),
                        &[("limit", "200"), ("page", &page.to_string())], None).await.map_err(fail)?;
                    let (next, count) = crate::rt::spawn_blocking(move |_| {
                        let result = response["results"].as_array().ok_or_else(|| fail("Invalid Audiobookshelf items"))?;
                        let count = result.len();
                        items.extend(result.iter().map(|item| (text(&item["id"]), text(&item["media"]["metadata"]["title"]))));
                        Ok::<_, LibError>((items, count))
                    }).await.map_err(fail)??;
                    items = next;
                    if count < 200 || items.len() >= 20000 { break; }
                }
            }
            crate::rt::spawn_blocking(move |_| {
                items.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
                items
            }).await.map_err(fail)
        }).await
    }

    pub fn shelf_episodes(&self, item: &str, policy: Policy) -> Result<Rows, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        let item = item.to_owned();
        super::super::shelf_browse::wait(async move { lib.shelf_episodes_async(item, policy).await })
    }

    pub async fn shelf_episodes_async(self: &Arc<Self>, item: String, policy: Policy) -> Result<Rows, LibError> {
        self.legacy_shelf_rows(format!("episodes:{item}"), policy, move |client| async move {
            let response = client.get_query_async(&format!("api/items/{}", segment(&item)), &[("expanded", "1")], None)
                .await.map_err(fail)?;
            crate::rt::spawn_blocking(move |_| {
                Ok(response["media"]["episodes"].as_array().ok_or_else(|| fail("Invalid Audiobookshelf podcast"))?
                    .iter().filter(|episode| !episode["audioFile"].is_null())
                    .map(|episode| (text(&episode["id"]), text(&episode["title"]))).collect())
            }).await.map_err(fail)?
        }).await
    }

    async fn legacy_shelf_rows<F>(self: &Arc<Self>, suffix: String, policy: Policy,
        fetch: impl FnOnce(Arc<Client>) -> F + Send) -> Result<Rows, LibError>
    where F: std::future::Future<Output = Result<Rows, LibError>> + Send {
        let lib = self.clone();
        let (namespace, generation) = crate::rt::spawn_blocking(move |_| {
            let slot = lib.shelf.read().unwrap();
            (slot.namespace.clone(), slot.generation.clone())
        }).await.map_err(fail)?;
        let cached = || {
            let key = format!("shelf:{namespace}:{suffix}");
            let generation = generation.clone();
            async move {
                let body = self.shelf_db_run(generation.clone(), move |blocking, db| {
                    db.spoken_response(blocking, &key).map_err(fail)?.ok_or(LibError::Offline)
                }).await?;
                let rows = crate::rt::spawn_blocking(move |_| serde_json::from_str::<Rows>(&body).map_err(fail))
                    .await.map_err(fail)??;
                self.shelf_result(generation, rows).await
            }
        };
        if policy == Policy::CacheOnly { return cached().await; }
        let fresh = async {
            let client = self.shelf_client_async().await?;
            let namespace = client.namespace_async().await.map_err(fail)?;
            let rows = fetch(client).await?;
            let (rows, body) = crate::rt::spawn_blocking(move |_| {
                let body = serde_json::to_string(&rows).map_err(fail)?;
                Ok::<_, LibError>((rows, body))
            }).await.map_err(fail)??;
            let key = format!("shelf:{namespace}:{suffix}");
            self.shelf_db_run(generation.clone(), move |blocking, db| db.put_spoken_response(blocking, &key, &body).map_err(fail)).await?;
            self.shelf_result(generation.clone(), rows).await
        }.await;
        // Preserve these diagnostic APIs' existing fallback policy, including
        // cache failure returning the original transport/publication error.
        match fresh { Ok(rows) => Ok(rows), Err(error) => cached().await.or(Err(error)) }
    }
}
