//! CRUD round trip against a real D1 database.
//!
//! Requires `D1_ACCOUNT_ID`, `D1_DATABASE_ID`, and `D1_API_TOKEN` (D1 Edit
//! permission); skips silently otherwise so `cargo test` stays green in
//! environments without credentials.
//!
//! The test drops and recreates all tables in the target database — point it
//! at a dedicated test database, never a production one.

use toasty::Db;

#[derive(Debug, toasty::Model)]
struct Todo {
    #[key]
    #[auto]
    id: u64,

    title: String,

    done: bool,
}

#[tokio::test]
async fn crud_round_trip_against_live_d1() {
    let driver = match toasty_driver_d1::D1::from_env() {
        Ok(driver) => driver,
        Err(_) => {
            eprintln!("skipping live D1 test: CLOUDFLARE_* env vars not set");
            return;
        }
    };

    let db = Db::builder()
        .models(toasty::models!(crate::*))
        .build(driver)
        .await
        .unwrap();

    db.reset_db().await.unwrap();
    db.push_schema().await.unwrap();

    // Create
    let mut handle = db.clone();
    let created = toasty::create!(Todo {
        title: "live spike",
        done: false
    })
    .exec(&mut handle)
    .await
    .unwrap();
    assert_eq!(created.title, "live spike");
    assert!(!created.done);

    // Read
    let all = Todo::all()
        .order_by(Todo::fields().id().asc())
        .exec(&mut handle)
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, created.id);

    // Update
    let mut todo = Todo::get_by_id(&mut handle, created.id).await.unwrap();
    toasty::update!(todo { done: true })
        .exec(&mut handle)
        .await
        .unwrap();
    let reloaded = Todo::get_by_id(&mut handle, created.id).await.unwrap();
    assert!(reloaded.done);

    // Delete
    Todo::delete_by_id(&mut handle, created.id).await.unwrap();
    let remaining = Todo::all().exec(&mut handle).await.unwrap();
    assert!(remaining.is_empty());
}
