use sqlx::migrate::Migrator;

pub static MIGRATOR: Migrator = sqlx::migrate!("./infra/migrations");
