use sqlx::{migrate::Migrator, postgres::PgPoolOptions};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrateFromSourceArgs {
    pub database_url: String,
    pub source: PathBuf,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<MigrateFromSourceArgs, String> {
    let mut args = args.into_iter();
    let mut database_url = None;
    let mut source = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--database-url" => {
                database_url = Some(
                    args.next()
                        .ok_or_else(|| "--database-url requires a value".to_owned())?,
                );
            }
            "--source" => {
                source = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--source requires a value".to_owned())?,
                ));
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }
    Ok(MigrateFromSourceArgs {
        database_url: database_url.ok_or_else(|| "--database-url is required".to_owned())?,
        source: source.ok_or_else(|| "--source is required".to_owned())?,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = parse_args(std::env::args().skip(1))
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let migrator = Migrator::new(Path::new(&args.source)).await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    migrator.run(&pool).await?;
    pool.close().await;
    Ok(())
}
