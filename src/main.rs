use std::env::{Args, args};

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

    let results = manager.search(&args().nth(1).unwrap(), Some(5)).await?;

    results.into_iter().for_each(|pair| {
        let item_path = pair.entry.entry.path;
        let item_name = item_path.file_name().unwrap().to_string_lossy();
        let parent_path = item_path.parent().unwrap().to_string_lossy();

        println!("{}\t{}", parent_path, item_name);
    });

    Ok(())
}
