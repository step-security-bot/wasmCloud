//! blobstore-s3 capability provider
//!
//! This capability provider exposes [S3](https://aws.amazon.com/s3/)-compatible object storage
//! (AKA "blob store") as a [wasmcloud capability](https://wasmcloud.com/docs/concepts/capabilities) which
//! can be used by actors on your lattice.
//!

use core::future::Future;
use core::pin::pin;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use async_nats::HeaderMap;
use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::create_bucket::{CreateBucketError, CreateBucketOutput};
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;
use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
use aws_sdk_s3::types::{Delete, Object, ObjectIdentifier};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio::sync::RwLock;
use tokio::{select, spawn};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, instrument, warn};
use wasmcloud_provider_sdk::provider::invocation_context;
use wasmcloud_provider_sdk::{
    get_connection, Context, LinkConfig, ProviderHandler, ProviderOperationResult,
};
use wrpc_transport::{AcceptedInvocation, Transmitter};

mod config;
pub use config::StorageConfig;

const ALIAS_PREFIX: &str = "alias_";

/// Blobstore S3 provider
///
/// This struct will be the target of generated implementations (via wit-provider-bindgen)
/// for the blobstore provider WIT contract
#[derive(Default, Clone)]
pub struct BlobstoreS3Provider {
    /// Per-actor storage for NATS connection clients
    actors: Arc<RwLock<HashMap<String, StorageClient>>>,
}

impl BlobstoreS3Provider {
    /// Retrieve the per-actor [`StorageClient`] for a given link context
    async fn client(&self, headers: Option<&HeaderMap>) -> Result<StorageClient> {
        if let Some(ref source_id) = headers
            .map(invocation_context)
            .and_then(|Context { actor, .. }| actor)
        {
            self.actors
                .read()
                .await
                .get(source_id)
                .with_context(|| format!("failed to lookup {source_id} configuration"))
                .cloned()
        } else {
            // TODO: Support a default here
            bail!("failed to lookup invocation source ID")
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn serve(&self, commands: impl Future<Output = ()>) -> anyhow::Result<()> {
        let connection = get_connection();
        let wrpc = connection.get_wrpc_client(connection.provider_key());
        let mut commands = pin!(commands);
        'outer: loop {
            use wrpc_interface_blobstore::Blobstore as _;
            let clear_container_invocations = wrpc.serve_clear_container().await.context(
                "failed to serve `wrpc:blobstore/blobstore.clear-container` invocations",
            )?;
            let mut clear_container_invocations = pin!(clear_container_invocations);

            let container_exists_invocations = wrpc.serve_container_exists().await.context(
                "failed to serve `wrpc:blobstore/blobstore.container-exists` invocations",
            )?;
            let mut container_exists_invocations = pin!(container_exists_invocations);

            let create_container_invocations = wrpc.serve_create_container().await.context(
                "failed to serve `wrpc:blobstore/blobstore.create-container` invocations",
            )?;
            let mut create_container_invocations = pin!(create_container_invocations);

            let delete_container_invocations = wrpc.serve_delete_container().await.context(
                "failed to serve `wrpc:blobstore/blobstore.delete-container` invocations",
            )?;
            let mut delete_container_invocations = pin!(delete_container_invocations);

            let get_container_info_invocations = wrpc.serve_get_container_info().await.context(
                "failed to serve `wrpc:blobstore/blobstore.get-container-info` invocations",
            )?;
            let mut get_container_info_invocations = pin!(get_container_info_invocations);

            let list_container_objects_invocations =
                wrpc.serve_list_container_objects().await.context(
                    "failed to serve `wrpc:blobstore/blobstore.list-container-objects` invocations",
                )?;
            let mut list_container_objects_invocations = pin!(list_container_objects_invocations);

            let copy_object_invocations = wrpc
                .serve_copy_object()
                .await
                .context("failed to serve `wrpc:blobstore/blobstore.copy-object` invocations")?;
            let mut copy_object_invocations = pin!(copy_object_invocations);

            let delete_object_invocations = wrpc
                .serve_delete_object()
                .await
                .context("failed to serve `wrpc:blobstore/blobstore.delete-object` invocations")?;
            let mut delete_object_invocations = pin!(delete_object_invocations);

            let delete_objects_invocations = wrpc
                .serve_delete_objects()
                .await
                .context("failed to serve `wrpc:blobstore/blobstore.delete-objects` invocations")?;
            let mut delete_objects_invocations = pin!(delete_objects_invocations);

            let get_container_data_invocations = wrpc.serve_get_container_data().await.context(
                "failed to serve `wrpc:blobstore/blobstore.get-container-data` invocations",
            )?;
            let mut get_container_data_invocations = pin!(get_container_data_invocations);

            let get_object_info_invocations = wrpc.serve_get_object_info().await.context(
                "failed to serve `wrpc:blobstore/blobstore.get-object-info` invocations",
            )?;
            let mut get_object_info_invocations = pin!(get_object_info_invocations);

            let has_object_invocations = wrpc
                .serve_has_object()
                .await
                .context("failed to serve `wrpc:blobstore/blobstore.has-object` invocations")?;
            let mut has_object_invocations = pin!(has_object_invocations);

            let move_object_invocations = wrpc
                .serve_move_object()
                .await
                .context("failed to serve `wrpc:blobstore/blobstore.move-object` invocations")?;
            let mut move_object_invocations = pin!(move_object_invocations);

            let write_container_data_invocations =
                wrpc.serve_write_container_data().await.context(
                    "failed to serve `wrpc:blobstore/blobstore.write-container-data` invocations",
                )?;
            let mut write_container_data_invocations = pin!(write_container_data_invocations);

            loop {
                select! {
                    invocation = clear_container_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_clear_container(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.clear-container` invocation")
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.clear-container` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    },
                    invocation = container_exists_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_container_exists(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.container-exists` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.container-exists` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = create_container_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_create_container(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.container-exists` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.container-exists` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = delete_container_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_delete_container(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.delete-container` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.delete-container` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = get_container_info_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_get_container_info(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.get-container-info` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.get-container-info` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = list_container_objects_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_list_container_objects(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.list-container-objects` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.list-container-objects` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = copy_object_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_copy_object(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.copy-object` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.copy-object` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = delete_object_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_delete_object(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.delete-object` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.delete-object` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = delete_objects_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_delete_objects(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.delete-objects` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.delete-objects` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = get_container_data_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_get_container_data(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.get-container-data` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.get-container-data` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = get_object_info_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_get_object_info(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.get-object-info` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.get-object-info` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = has_object_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_has_object(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.has-object` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.has-object` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = move_object_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_move_object(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.move-object` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.move-object` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    invocation = write_container_data_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_write_container_data(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:blobstore/blobstore.write-container-data` invocation") ;
                            },
                            None => {
                                warn!("`wrpc:blobstore/blobstore.write-container-data` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            },
                        }
                    },
                    _ = &mut commands => {
                        debug!("shutdown command received");
                        return Ok(())
                    }
                }
            }
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_clear_container<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: container,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, String, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    let bucket = client.unalias(&container);
                    let objects = client
                        .list_container_objects(bucket, None, None)
                        .await
                        .context("failed to list container objects")?;
                    client.delete_objects(bucket, objects).await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_container_exists<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: container,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, String, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client.container_exists(client.unalias(&container)).await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_create_container<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: container,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, String, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client.create_container(client.unalias(&container)).await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_delete_container<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: container,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, String, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client.delete_container(client.unalias(&container)).await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_get_container_info<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: container,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, String, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client.get_container_info(client.unalias(&container)).await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[allow(clippy::type_complexity)]
    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_list_container_objects<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (container, limit, offset),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, Option<u64>, Option<u64>), Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .list_container_objects(client.unalias(&container), limit, offset)
                        .await
                        .map(Vec::from_iter)
                        .map(Some)
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_copy_object<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (src, dest),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<
            Option<HeaderMap>,
            (
                wrpc_interface_blobstore::ObjectId,
                wrpc_interface_blobstore::ObjectId,
            ),
            Tx,
        >,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    let src_bucket = client.unalias(&src.container);
                    let dest_bucket = client.unalias(&dest.container);
                    client
                        .copy_object(src_bucket, &src.object, dest_bucket, &dest.object)
                        .await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_delete_object<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: id,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, wrpc_interface_blobstore::ObjectId, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .delete_object(client.unalias(&id.container), id.object)
                        .await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_delete_objects<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (container, objects),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, Vec<String>), Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .delete_objects(client.unalias(&container), objects)
                        .await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_get_container_data<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (id, start, end),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<
            Option<HeaderMap>,
            (wrpc_interface_blobstore::ObjectId, u64, u64),
            Tx,
        >,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let limit = end
                        .checked_sub(start)
                        .context("`end` must be greater than `start`")?;
                    let client = self.client(context.as_ref()).await?;
                    let bucket = client.unalias(&id.container);
                    let GetObjectOutput { body, .. } = client
                        .s3_client
                        .get_object()
                        .bucket(bucket)
                        .key(id.object)
                        .range(format!("bytes={start}-{end}"))
                        .send()
                        .await
                        .context("failed to get object")?;
                    let data =
                        ReaderStream::new(body.into_async_read().take(limit)).map(move |buf| {
                            let buf = buf.context("failed to read chunk")?;
                            // TODO: Remove the need for this wrapping
                            Ok(buf
                                .into_iter()
                                .map(wrpc_transport::Value::U8)
                                .map(Some)
                                .collect())
                        });
                    anyhow::Ok(wrpc_transport::Value::Stream(Box::pin(data)))
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_get_object_info<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: id,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, wrpc_interface_blobstore::ObjectId, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .get_object_info(client.unalias(&id.container), &id.object)
                        .await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_has_object<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: id,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, wrpc_interface_blobstore::ObjectId, Tx>,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .has_object(client.unalias(&id.container), &id.object)
                        .await
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_move_object<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (src, dest),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<
            Option<HeaderMap>,
            (
                wrpc_interface_blobstore::ObjectId,
                wrpc_interface_blobstore::ObjectId,
            ),
            Tx,
        >,
    ) {
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    let src_bucket = client.unalias(&src.container);
                    let dest_bucket = client.unalias(&dest.container);
                    client
                        .copy_object(src_bucket, &src.object, dest_bucket, &dest.object)
                        .await
                        .context("failed to copy object")?;
                    client
                        .delete_object(src_bucket, src.object)
                        .await
                        .context("failed to delete source object")
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(
        level = "debug",
        skip(self, result_subject, error_subject, transmitter, data)
    )]
    async fn serve_write_container_data<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (id, data),
            result_subject,
            error_subject,
            transmitter,
            ..
        }: AcceptedInvocation<
            Option<HeaderMap>,
            (
                wrpc_interface_blobstore::ObjectId,
                impl Stream<Item = anyhow::Result<Bytes>> + Send,
            ),
            Tx,
        >,
    ) {
        // TODO: Stream value to S3
        let data: BytesMut = match data.try_collect().await {
            Ok(data) => data,
            Err(err) => {
                error!(?err, "failed to receive value");
                if let Err(err) = transmitter
                    .transmit_static(error_subject, err.to_string())
                    .await
                {
                    error!(?err, "failed to transmit error")
                }
                return;
            }
        };
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                async {
                    let client = self.client(context.as_ref()).await?;
                    client
                        .s3_client
                        .put_object()
                        .bucket(client.unalias(&id.container))
                        .key(&id.object)
                        .body(data.freeze().into())
                        .send()
                        .await
                        .context("failed to put object")?;
                    anyhow::Ok(())
                }
                .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }
}

/// Handle provider control commands
/// put_link (new actor link command), del_link (remove link command), and shutdown
#[async_trait]
impl ProviderHandler for BlobstoreS3Provider {
    /// Provider should perform any operations needed for a new link,
    /// including setting up per-actor resources, and checking authorization.
    /// If the link is allowed, return true, otherwise return false to deny the link.
    async fn receive_link_config_as_target(
        &self,
        link_config: impl LinkConfig,
    ) -> ProviderOperationResult<()> {
        let source_id = link_config.get_source_id();
        let config_values = link_config.get_config();

        // Build storage config
        let config = match StorageConfig::from_values(config_values) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, %source_id, "failed to build storage config");
                return Err(anyhow!(e).context("failed to build source config").into());
            }
        };

        let link = StorageClient::new(config, config_values).await;

        let mut update_map = self.actors.write().await;
        update_map.insert(source_id.to_string(), link);

        Ok(())
    }

    /// Handle notification that a link is dropped: close the connection
    async fn delete_link(&self, source_id: &str) -> ProviderOperationResult<()> {
        let mut aw = self.actors.write().await;
        aw.remove(source_id);
        Ok(())
    }

    /// Handle shutdown request by closing all connections
    async fn shutdown(&self) -> ProviderOperationResult<()> {
        let mut aw = self.actors.write().await;
        // empty the actor link data and stop all servers
        aw.drain();
        Ok(())
    }
}

#[derive(Clone)]
pub struct StorageClient {
    s3_client: aws_sdk_s3::Client,
    aliases: Arc<HashMap<String, String>>,
}

impl StorageClient {
    pub async fn new(config: StorageConfig, config_values: &HashMap<String, String>) -> Self {
        let tls_use_webpki_roots = config.tls_use_webpki_roots;
        let mut aliases = config.aliases.clone();
        let mut s3_config = aws_sdk_s3::Config::from(&config.configure_aws().await)
            .to_builder()
            // Since minio requires force path style,
            // turn it on since it's disabled by default
            // due to deprecation by AWS.
            // https://github.com/awslabs/aws-sdk-rust/issues/390
            .force_path_style(true);

        // In test configuration(s) we can use a client that does not require native roots
        // so that requests will work in a hermetic build environment
        if let Some(true) = tls_use_webpki_roots {
            use aws_smithy_runtime::client::http::hyper_014::HyperClientBuilder;
            let https_connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_or_http()
                .enable_all_versions()
                .build();
            let http_client = HyperClientBuilder::new().build(https_connector);
            s3_config = s3_config.http_client(http_client);
        }
        let s3_config = s3_config.build();

        let s3_client = aws_sdk_s3::Client::from_conf(s3_config);

        // Process aliases
        for (k, v) in config_values.iter() {
            if let Some(alias) = k.strip_prefix(ALIAS_PREFIX) {
                if alias.is_empty() || v.is_empty() {
                    error!("invalid bucket alias_ key and value must not be empty");
                } else {
                    aliases.insert(alias.to_string(), v.to_string());
                }
            }
        }

        StorageClient {
            s3_client,
            aliases: Arc::new(aliases),
        }
    }

    /// perform alias lookup on bucket name
    /// This can be used either for giving shortcuts to actors in the linkdefs, for example:
    /// - actor could use bucket names "alias_today", "alias_images", etc. and the linkdef aliases
    ///   will remap them to the real bucket name
    /// The 'alias_' prefix is not required, so this also works as a general redirect capability
    pub fn unalias<'n, 's: 'n>(&'s self, bucket_or_alias: &'n str) -> &'n str {
        debug!(%bucket_or_alias, aliases = ?self.aliases);
        let name = bucket_or_alias
            .strip_prefix(ALIAS_PREFIX)
            .unwrap_or(bucket_or_alias);
        if let Some(name) = self.aliases.get(name) {
            name.as_ref()
        } else {
            name
        }
    }

    /// Check whether a container exists
    #[instrument(level = "debug", skip(self))]
    pub async fn container_exists(&self, bucket: &str) -> anyhow::Result<bool> {
        match self.s3_client.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(true),
            Err(se) => match se.into_service_error() {
                HeadBucketError::NotFound(_) => Ok(false),
                err => {
                    error!(?err, "Unable to head bucket");
                    bail!(anyhow!(err).context("failed to `head` bucket"))
                }
            },
        }
    }

    /// Create a bucket
    #[instrument(level = "debug", skip(self))]
    pub async fn create_container(&self, bucket: &str) -> anyhow::Result<()> {
        match self.s3_client.create_bucket().bucket(bucket).send().await {
            Ok(CreateBucketOutput { location, .. }) => {
                debug!(?location, "bucket created");
                Ok(())
            }
            Err(se) => match se.into_service_error() {
                CreateBucketError::BucketAlreadyOwnedByYou(..) => Ok(()),
                err => {
                    error!(?err, "failed to create bucket");
                    bail!(anyhow!(err).context("failed to create bucket"))
                }
            },
        }
    }

    #[instrument(level = "debug", skip(self))]
    pub async fn get_container_info(
        &self,
        bucket: &str,
    ) -> anyhow::Result<wrpc_interface_blobstore::ContainerMetadata> {
        match self.s3_client.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(wrpc_interface_blobstore::ContainerMetadata {
                // unfortunately, HeadBucketOut doesn't include any information
                // so we can't fill in creation date
                created_at: 0,
            }),
            Err(se) => match se.into_service_error() {
                HeadBucketError::NotFound(_) => {
                    error!("bucket [{bucket}] not found");
                    bail!("bucket [{bucket}] not found")
                }
                e => {
                    error!("unexpected error: {e}");
                    bail!("unexpected error: {e}");
                }
            },
        }
    }

    #[instrument(level = "debug", skip(self))]
    pub async fn list_container_objects(
        &self,
        bucket: &str,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> anyhow::Result<impl Iterator<Item = String>> {
        // TODO: Stream names
        match self
            .s3_client
            .list_objects_v2()
            .bucket(bucket)
            .set_max_keys(limit.map(|limit| limit.try_into().unwrap_or(i32::MAX)))
            .send()
            .await
        {
            Ok(ListObjectsV2Output { contents, .. }) => Ok(contents
                .into_iter()
                .flatten()
                .flat_map(|Object { key, .. }| key)
                .skip(offset.unwrap_or_default().try_into().unwrap_or(usize::MAX))
                .take(limit.unwrap_or(u64::MAX).try_into().unwrap_or(usize::MAX))),
            Err(SdkError::ServiceError(err)) => {
                error!(?err, "service error");
                bail!(anyhow!("{err:?}").context("service error"))
            }
            Err(err) => {
                error!(%err, "unexpected error");
                bail!(anyhow!("{err:?}").context("unexpected error"))
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    pub async fn copy_object(
        &self,
        src_bucket: &str,
        src_key: &str,
        dest_bucket: &str,
        dest_key: &str,
    ) -> anyhow::Result<()> {
        self.s3_client
            .copy_object()
            .copy_source(format!("{src_bucket}/{src_key}"))
            .bucket(dest_bucket)
            .key(dest_key)
            .send()
            .await
            .context("failed to copy object")?;
        Ok(())
    }

    #[instrument(level = "debug", skip(self, object))]
    pub async fn delete_object(&self, container: &str, object: String) -> anyhow::Result<()> {
        self.s3_client
            .delete_object()
            .bucket(container)
            .key(object)
            .send()
            .await
            .context("failed to delete object")?;
        Ok(())
    }

    #[instrument(level = "debug", skip(self, objects))]
    pub async fn delete_objects(
        &self,
        container: &str,
        objects: impl IntoIterator<Item = String>,
    ) -> anyhow::Result<()> {
        let objects: Vec<_> = objects
            .into_iter()
            .map(|key| ObjectIdentifier::builder().key(key).build())
            .collect::<Result<_, _>>()
            .context("failed to build object identifier list")?;
        if objects.is_empty() {
            debug!("no objects to delete, return");
            return Ok(());
        }
        let delete = Delete::builder()
            .set_objects(Some(objects))
            .build()
            .context("failed to build `delete_objects` command")?;
        let out = self
            .s3_client
            .delete_objects()
            .bucket(container)
            .delete(delete)
            .send()
            .await
            .context("failed to delete objects")?;
        let errs = out.errors();
        if !errs.is_empty() {
            bail!("failed with errors {errs:?}")
        }
        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    pub async fn delete_container(&self, bucket: &str) -> anyhow::Result<()> {
        match self.s3_client.delete_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(()),
            Err(SdkError::ServiceError(err)) => {
                bail!("{err:?}")
            }
            Err(err) => {
                error!(%err, "unexpected error");
                bail!(err)
            }
        }
    }

    /// Find out whether object exists
    #[instrument(level = "debug", skip(self))]
    pub async fn has_object(&self, bucket: &str, key: &str) -> anyhow::Result<bool> {
        match self
            .s3_client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(se) => match se.into_service_error() {
                HeadObjectError::NotFound(_) => Ok(false),
                err => {
                    error!(
                        %err,
                        "unexpected error for object_exists"
                    );
                    bail!(anyhow!(err).context("unexpected error for object_exists"))
                }
            },
        }
    }

    /// Retrieves metadata about the object
    #[instrument(level = "debug", skip(self))]
    pub async fn get_object_info(
        &self,
        bucket: &str,
        key: &str,
    ) -> anyhow::Result<wrpc_interface_blobstore::ObjectMetadata> {
        match self
            .s3_client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(HeadObjectOutput { content_length, .. }) => {
                Ok(wrpc_interface_blobstore::ObjectMetadata {
                    // NOTE: The `created_at` value is not reported by S3
                    created_at: 0,
                    size: content_length
                        .and_then(|v| v.try_into().ok())
                        .unwrap_or_default(),
                })
            }
            Err(se) => match se.into_service_error() {
                HeadObjectError::NotFound(_) => {
                    error!("object [{bucket}/{key}] not found");
                    bail!("object [{bucket}/{key}] not found")
                }
                err => {
                    error!("get_object_metadata failed for object [{bucket}/{key}]: {err}",);
                    bail!(anyhow!(err).context(format!(
                        "get_object_metadata failed for object [{bucket}/{key}]"
                    )))
                }
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn aliases() {
        let client = StorageClient::new(
            StorageConfig::default(),
            &HashMap::from([(format!("{ALIAS_PREFIX}foo"), "bar".into())]),
        )
        .await;

        // no alias
        assert_eq!(client.unalias("boo"), "boo");
        // alias without prefix
        assert_eq!(client.unalias("foo"), "bar");
        // alias with prefix
        assert_eq!(client.unalias(&format!("{}foo", ALIAS_PREFIX)), "bar");
        // undefined alias
        assert_eq!(client.unalias(&format!("{}baz", ALIAS_PREFIX)), "baz");
    }
}
