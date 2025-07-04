// src/db.rs

use tokio_rusqlite::Connection;

// This function will be called once when the application starts.
// It connects to the SQLite database and creates the `orders` table if it doesn't exist.
pub async fn init_db(db_path: &str) -> Result<Connection, tokio_rusqlite::Error> {
    // Open a connection to the SQLite database file.
    // The database file will be created if it does not exist.
    let conn = Connection::open(db_path).await?;

    // Execute the SQL command to create our table.
    // `IF NOT EXISTS` ensures that we don't get an error if the table already exists.
    conn.call(|conn| {
        conn.execute(
            "
            CREATE TABLE IF NOT EXISTS orders (
                order_id            TEXT PRIMARY KEY,
                service_id          TEXT NOT NULL,
                link                TEXT NOT NULL,
                quantity            INTEGER NOT NULL,
                usd_price           REAL NOT NULL,
                xmr_amount          REAL NOT NULL,
                payment_address     TEXT NOT NULL UNIQUE,
                status              TEXT NOT NULL,
                spotboost_order_id  INTEGER,
                created_at          TEXT NOT NULL,
                last_updated        TEXT NOT NULL
            )
            ",
            [], // No parameters for this query
        )?;
        Ok(())
    })
    .await?;
    
    // Print a confirmation message to the console.
    tracing::info!("Database connection established and `orders` table verified.");
    
    // Return the connection object.
    Ok(conn)
}
