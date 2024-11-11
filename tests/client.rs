use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use iroh_blobs::{
    store::ExportMode,
    util::fs::{key_to_path, path_to_key},
};
use iroh_docs::store::Query;
use rand::RngCore;
use tokio::io::AsyncWriteExt;
use util::Node;

mod util;

/// Test that closing a doc does not close other instances.
#[tokio::test]
async fn test_doc_close() -> Result<()> {
    let _guard = iroh_test::logging::setup();

    let node = Node::memory().spawn().await?;
    let author = node.docs().author_default().await?;
    // open doc two times
    let doc1 = node.docs().create().await?;
    let doc2 = node.docs().open(doc1.id()).await?.expect("doc to exist");
    // close doc1 instance
    doc1.close().await?;
    // operations on doc1 now fail.
    assert!(doc1.set_bytes(author, "foo", "bar").await.is_err());
    // dropping doc1 will close the doc if not already closed
    // wait a bit because the close-on-drop spawns a task for which we cannot track completion.
    drop(doc1);
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // operations on doc2 still succeed
    doc2.set_bytes(author, "foo", "bar").await?;
    Ok(())
}

#[tokio::test]
async fn test_doc_import_export() -> Result<()> {
    let _guard = iroh_test::logging::setup();

    let node = Node::memory().spawn().await?;

    // create temp file
    let temp_dir = tempfile::tempdir().context("tempdir")?;

    let in_root = temp_dir.path().join("in");
    tokio::fs::create_dir_all(in_root.clone())
        .await
        .context("create dir all")?;
    let out_root = temp_dir.path().join("out");

    let path = in_root.join("test");

    let size = 100;
    let mut buf = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut buf);
    let mut file = tokio::fs::File::create(path.clone())
        .await
        .context("create file")?;
    file.write_all(&buf.clone()).await.context("write_all")?;
    file.flush().await.context("flush")?;

    // create doc & author
    let client = node.client();
    let docs_client = client.docs();
    let doc = docs_client.create().await.context("doc create")?;
    let author = docs_client.author_create().await.context("author create")?;

    // import file
    let import_outcome = doc
        .import_file(
            author,
            path_to_key(path.clone(), None, Some(in_root))?,
            path,
            true,
        )
        .await
        .context("import file")?
        .finish()
        .await
        .context("import finish")?;

    // export file
    let entry = doc
        .get_one(Query::author(author).key_exact(import_outcome.key))
        .await
        .context("get one")?
        .unwrap();
    let key = entry.key().to_vec();
    let export_outcome = doc
        .export_file(
            entry,
            key_to_path(key, None, Some(out_root))?,
            ExportMode::Copy,
        )
        .await
        .context("export file")?
        .finish()
        .await
        .context("export finish")?;

    let got_bytes = tokio::fs::read(export_outcome.path)
        .await
        .context("tokio read")?;
    assert_eq!(buf, got_bytes);

    Ok(())
}

#[tokio::test]
async fn test_authors() -> Result<()> {
    let node = Node::memory().spawn().await?;

    // default author always exists
    let authors: Vec<_> = node.docs().author_list().await?.try_collect().await?;
    assert_eq!(authors.len(), 1);
    let default_author = node.docs().author_default().await?;
    assert_eq!(authors, vec![default_author]);

    let author_id = node.docs().author_create().await?;

    let authors: Vec<_> = node.docs().author_list().await?.try_collect().await?;
    assert_eq!(authors.len(), 2);

    let author = node
        .docs()
        .author_export(author_id)
        .await?
        .expect("should have author");
    node.docs().author_delete(author_id).await?;
    let authors: Vec<_> = node.docs().author_list().await?.try_collect().await?;
    assert_eq!(authors.len(), 1);

    node.docs().author_import(author).await?;

    let authors: Vec<_> = node.docs().author_list().await?.try_collect().await?;
    assert_eq!(authors.len(), 2);

    assert!(node.docs().author_default().await? != author_id);
    node.docs().author_set_default(author_id).await?;
    assert_eq!(node.docs().author_default().await?, author_id);

    Ok(())
}

#[tokio::test]
async fn test_default_author_memory() -> Result<()> {
    let iroh = Node::memory().spawn().await?;
    let author = iroh.docs().author_default().await?;
    assert!(iroh.docs().author_export(author).await?.is_some());
    assert!(iroh.docs().author_delete(author).await.is_err());
    Ok(())
}

#[cfg(feature = "fs-store")]
#[tokio::test]
async fn test_default_author_persist() -> Result<()> {
    use crate::util::path::IrohPaths;

    let _guard = iroh_test::logging::setup();

    let iroh_root_dir = tempfile::TempDir::new().unwrap();
    let iroh_root = iroh_root_dir.path();

    // check that the default author exists and cannot be deleted.
    let default_author = {
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await
            .unwrap();
        let author = iroh.authors().default().await.unwrap();
        assert!(iroh.authors().export(author).await.unwrap().is_some());
        assert!(iroh.authors().delete(author).await.is_err());
        iroh.shutdown().await.unwrap();
        author
    };

    // check that the default author is persisted across restarts.
    {
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await
            .unwrap();
        let author = iroh.authors().default().await.unwrap();
        assert_eq!(author, default_author);
        assert!(iroh.authors().export(author).await.unwrap().is_some());
        assert!(iroh.authors().delete(author).await.is_err());
        iroh.shutdown().await.unwrap();
    };

    // check that a new default author is created if the default author file is deleted
    // manually.
    let default_author = {
        tokio::fs::remove_file(IrohPaths::DefaultAuthor.with_root(iroh_root))
            .await
            .unwrap();
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await
            .unwrap();
        let author = iroh.authors().default().await.unwrap();
        assert!(author != default_author);
        assert!(iroh.authors().export(author).await.unwrap().is_some());
        assert!(iroh.authors().delete(author).await.is_err());
        iroh.shutdown().await.unwrap();
        author
    };

    // check that the node fails to start if the default author is missing from the docs store.
    {
        let mut docs_store =
            iroh_docs::store::fs::Store::persistent(IrohPaths::DocsDatabase.with_root(iroh_root))
                .unwrap();
        docs_store.delete_author(default_author).unwrap();
        docs_store.flush().unwrap();
        drop(docs_store);
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await;
        assert!(iroh.is_err());

        // somehow the blob store is not shutdown correctly (yet?) on macos.
        // so we give it some time until we find a proper fix.
        #[cfg(target_os = "macos")]
        tokio::time::sleep(Duration::from_secs(1)).await;

        tokio::fs::remove_file(IrohPaths::DefaultAuthor.with_root(iroh_root))
            .await
            .unwrap();
        drop(iroh);
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await;
        assert!(iroh.is_ok());
        iroh.unwrap().shutdown().await.unwrap();
    }

    // check that the default author can be set manually and is persisted.
    let default_author = {
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await
            .unwrap();
        let author = iroh.authors().create().await.unwrap();
        iroh.authors().set_default(author).await.unwrap();
        assert_eq!(iroh.authors().default().await.unwrap(), author);
        iroh.shutdown().await.unwrap();
        author
    };
    {
        let iroh = Node::persistent(iroh_root)
            .await
            .unwrap()
            .enable_docs()
            .spawn()
            .await
            .unwrap();
        assert_eq!(iroh.authors().default().await.unwrap(), default_author);
        iroh.shutdown().await.unwrap();
    }

    Ok(())
}
