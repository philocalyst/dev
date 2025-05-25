use anyhow::{Error, Result};
use bitcode;
use dev::*;
use tokio;
use tokio::fs;

#[tokio::main]
async fn main() {
    run().await.unwrap();
}

async fn run() -> Result<(), Error> {
    let manager = DevDocsManager::new().unwrap();

    manager.init().await?;
    manager.add_doc("rust").await?;

    let data_dir = manager.data_dir;

    Ok(())
}
