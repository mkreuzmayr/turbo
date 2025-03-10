use std::pin::Pin;

use anyhow::{bail, Result};
use futures::{prelude::*, Stream};
use tokio::sync::mpsc::Sender;
use tokio_stream::wrappers::ReceiverStream;
use turbo_tasks::{CollectiblesSource, State, TransientInstance};
use turbopack_core::{
    issue::{IssueVc, PlainIssueReadRef},
    version::{
        NotFoundVersionVc, PartialUpdate, TotalUpdate, Update, UpdateReadRef, VersionVc,
        VersionedContent,
    },
};

use crate::source::resolve::{ResolveSourceRequestResult, ResolveSourceRequestResultVc};

type GetContentFn = Box<dyn Fn() -> ResolveSourceRequestResultVc + Send + Sync>;

async fn peek_issues<T: CollectiblesSource + Copy>(source: T) -> Result<Vec<PlainIssueReadRef>> {
    let captured = IssueVc::peek_issues_with_path(source).await?.await?;

    captured.get_plain_issues().await
}

fn extend_issues(issues: &mut Vec<PlainIssueReadRef>, new_issues: Vec<PlainIssueReadRef>) {
    for issue in new_issues {
        if issues.contains(&issue) {
            continue;
        }

        issues.push(issue);
    }
}

#[turbo_tasks::function]
async fn get_update_stream_item(
    from: VersionStateVc,
    get_content: TransientInstance<GetContentFn>,
) -> Result<UpdateStreamItemVc> {
    let content = get_content();

    match &*content.await? {
        ResolveSourceRequestResult::Static(static_content, _) => {
            let resolved_content = static_content.await?.content;
            let from = from.get();
            let update = resolved_content.update(from);

            let mut plain_issues = peek_issues(update).await?;
            extend_issues(&mut plain_issues, peek_issues(content).await?);

            Ok(UpdateStreamItem {
                update: update.await?,
                issues: plain_issues,
            }
            .cell())
        }
        _ => {
            let plain_issues = peek_issues(content).await?;

            let update = if plain_issues.is_empty() {
                // Client requested a non-existing asset
                // It might be removed in meantime, reload client
                // TODO add special instructions for removed assets to handled it in a better
                // way
                Update::Total(TotalUpdate {
                    to: NotFoundVersionVc::new().into(),
                })
                .cell()
            } else {
                Update::None.cell()
            };

            Ok(UpdateStreamItem {
                update: update.await?,
                issues: plain_issues,
            }
            .cell())
        }
    }
}

#[turbo_tasks::function]
async fn compute_update_stream(
    from: VersionStateVc,
    get_content: TransientInstance<GetContentFn>,
    sender: TransientInstance<Sender<UpdateStreamItemReadRef>>,
) -> Result<()> {
    let item = get_update_stream_item(from, get_content)
        .strongly_consistent()
        .await?;

    if sender.send(item).await.is_err() {
        bail!("channel closed");
    }

    Ok(())
}

#[turbo_tasks::value]
struct VersionState {
    inner: State<VersionVc>,
}

#[turbo_tasks::value_impl]
impl VersionStateVc {
    #[turbo_tasks::function]
    async fn get(self) -> Result<VersionVc> {
        let this = self.await?;
        let version = *this.inner.get();
        Ok(version)
    }
}

impl VersionStateVc {
    async fn new(inner: VersionVc) -> Result<Self> {
        let inner = inner.cell_local().await?;
        Ok(Self::cell(VersionState {
            inner: State::new(inner),
        }))
    }

    async fn set(&self, new_inner: VersionVc) -> Result<()> {
        let this = self.await?;
        let new_inner = new_inner.cell_local().await?;
        this.inner.set(new_inner);
        Ok(())
    }
}

pub(super) struct UpdateStream(Pin<Box<dyn Stream<Item = UpdateStreamItemReadRef> + Send + Sync>>);

impl UpdateStream {
    pub async fn new(get_content: TransientInstance<GetContentFn>) -> Result<UpdateStream> {
        let (sx, rx) = tokio::sync::mpsc::channel(32);

        let content = get_content();
        // We can ignore issues reported in content here since [compute_update_stream]
        // will handle them
        let version = match &*content.await? {
            ResolveSourceRequestResult::Static(static_content, _) => {
                static_content.await?.content.version()
            }
            _ => NotFoundVersionVc::new().into(),
        };
        let version_state = VersionStateVc::new(version).await?;

        compute_update_stream(version_state, get_content, TransientInstance::new(sx));

        let mut last_had_issues = false;

        let stream = ReceiverStream::new(rx).filter_map(move |update| {
            let has_issues = !update.issues.is_empty();
            let issues_changed = has_issues != last_had_issues;
            last_had_issues = has_issues;

            async move {
                match &*update.update {
                    Update::Partial(PartialUpdate { to, .. })
                    | Update::Total(TotalUpdate { to }) => {
                        version_state
                            .set(*to)
                            .await
                            .expect("failed to update version");

                        Some(update)
                    }
                    // Do not propagate empty updates.
                    Update::None => {
                        if has_issues || issues_changed {
                            Some(update)
                        } else {
                            None
                        }
                    }
                }
            }
        });

        Ok(UpdateStream(Box::pin(stream)))
    }
}

impl Stream for UpdateStream {
    type Item = UpdateStreamItemReadRef;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0).poll_next(cx)
    }
}

#[turbo_tasks::value(serialization = "none")]
pub struct UpdateStreamItem {
    pub update: UpdateReadRef,
    pub issues: Vec<PlainIssueReadRef>,
}
