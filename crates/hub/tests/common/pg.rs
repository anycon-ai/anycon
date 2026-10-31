//! 集成测试里可选的 Postgres 存储：设了 `ANYCON_TEST_POSTGRES_URL`，用 [`TestHub`](super::TestHub) 起的 hub 就把数据放在
//! 这个库里的一个临时 schema 里（`ANYCON_DATABASE_URL` 的 `options=-c search_path=…`），hub 退出后删掉 schema。
//! 没设就是 SQLite，和以前一样。单元测试那边的同类设施在 `src/store/conformance.rs`。

use sqlx::postgres::PgPoolOptions;
use sqlx::AssertSqlSafe;

/// 指向测试用 Postgres 的环境变量。
pub const PG_URL_ENV: &str = "ANYCON_TEST_POSTGRES_URL";

/// 测试库里的一个临时 schema；drop 时（包括测试 panic）删掉。
pub struct PgSchema {
    base_url: String,
    schema: String,
}

impl PgSchema {
    /// `ANYCON_TEST_POSTGRES_URL` 设了就建一个新的 schema；没设返回 `None`。设了但连不上直接 panic：要跑 Postgres 就别悄悄退回 SQLite。
    pub async fn from_env() -> Option<Self> {
        let base_url = std::env::var(PG_URL_ENV).ok().map(|u| u.trim().to_string()).filter(|u| !u.is_empty())?;
        let schema = format!("t_{}", anycon_hub::util::new_id());
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&base_url)
            .await
            .unwrap_or_else(|e| panic!("{PG_URL_ENV} is set but the database is not reachable: {e}"));
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\""))).execute(&pool).await.unwrap();
        pool.close().await;
        Some(Self { base_url, schema })
    }

    /// 给 hub 的 `ANYCON_DATABASE_URL`：测试库的 URL 加上让这条连接的 `search_path` 指向临时 schema 的参数。
    pub fn database_url(&self) -> String {
        let mut url = url::Url::parse(&self.base_url).expect("the test database URL is valid");
        url.query_pairs_mut().append_pair("options", &format!("-c search_path={}", self.schema));
        url.to_string()
    }

    /// 把一张表改名（模拟库出了故障：之后碰这张表的查询都报错）。表名是测试里写死的字面量。
    pub async fn break_table(&self, table: &str) {
        let pool = PgPoolOptions::new().max_connections(1).connect(&self.base_url).await.unwrap();
        let sql = AssertSqlSafe(format!("ALTER TABLE \"{}\".\"{table}\" RENAME TO \"{table}_broken\"", self.schema));
        sqlx::query(sql).execute(&pool).await.unwrap();
        pool.close().await;
    }
}

impl Drop for PgSchema {
    fn drop(&mut self) {
        // 在自己的线程和运行时里删（drop 可能在任何运行时的线程上，也可能是 panic 展开）。hub 还开着连接，可能有没结束的事务：
        // 先把握着这个 schema 里的表的锁的会话踢掉再删，免得 DROP 一直等；`lock_timeout` 兜底。
        let (url, schema) = (self.base_url.clone(), self.schema.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(async {
                let Ok(pool) = PgPoolOptions::new().max_connections(1).connect(&url).await else { return };
                let _ = sqlx::query(
                    "SELECT pg_terminate_backend(l.pid) FROM pg_locks l \
                     JOIN pg_class c ON c.oid = l.relation JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND l.pid <> pg_backend_pid()",
                )
                .bind(&schema)
                .execute(&pool)
                .await;
                let _ = sqlx::query("SET lock_timeout = '10s'").execute(&pool).await;
                let _ = sqlx::query(AssertSqlSafe(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")))
                    .execute(&pool)
                    .await;
                pool.close().await;
            });
        })
        .join();
    }
}
