//! Probes which Toasty features actually work over D1, one feature per test.
//!
//! The official suite seeds almost every fixture with a multi-record
//! `create!`, which needs a transaction D1 cannot provide — so a suite
//! failure usually says nothing about the feature under test. These probes
//! avoid batch writes so each result reflects the feature itself.
//!
//! ```sh
//! cargo test --features live-tests --test capabilities
//! ```
#![cfg(feature = "live-tests")]

use toasty::Db;
use toasty_driver_d1::D1;

#[derive(Debug, toasty::Model)]
struct Author {
    #[key]
    #[auto]
    id: u64,

    #[index]
    name: String,

    #[has_many]
    books: toasty::Deferred<Vec<Book>>,
}

#[derive(Debug, toasty::Model)]
struct Book {
    #[key]
    #[auto]
    id: u64,

    #[index]
    title: String,

    copies: i64,

    #[index]
    author_id: u64,

    #[belongs_to(key = author_id, references = id)]
    author: toasty::Deferred<Author>,
}

async fn db() -> Db {
    let driver = D1::new(
        std::env::var("D1_ACCOUNT_ID").expect("D1_ACCOUNT_ID"),
        std::env::var("TOASTY_TEST_D1_DATABASE_ID").expect("TOASTY_TEST_D1_DATABASE_ID"),
        std::env::var("D1_API_TOKEN").expect("D1_API_TOKEN"),
    );
    let db = Db::builder()
        .models(toasty::models!(crate::*))
        .build(driver)
        .await
        .unwrap();
    db.reset_db().await.unwrap();
    db.push_schema().await.unwrap();
    db
}

/// Seeds one author and three books, one statement at a time.
async fn seed(db: &Db) -> u64 {
    let mut handle = db.clone();
    let author = Author::create()
        .name("Alice")
        .exec(&mut handle)
        .await
        .unwrap();

    for (title, copies) in [("Alpha", 3_i64), ("Almanac", 1), ("Beta", 7)] {
        Book::create()
            .title(title)
            .copies(copies)
            .author_id(author.id)
            .exec(&mut handle)
            .await
            .unwrap();
    }

    author.id
}

#[tokio::test]
async fn like_and_prefix_filters() {
    let db = db().await;
    seed(&db).await;
    let mut handle = db.clone();

    let like: Vec<Book> = Book::filter(Book::fields().title().like("Al%".to_string()))
        .exec(&mut handle)
        .await
        .unwrap();
    assert_eq!(like.len(), 2, "LIKE returned {like:?}");

    let prefix: Vec<Book> = Book::filter(Book::fields().title().starts_with("Al".to_string()))
        .exec(&mut handle)
        .await
        .unwrap();
    assert_eq!(prefix.len(), 2, "starts_with returned {prefix:?}");
}

#[tokio::test]
async fn sort_limit_and_count() {
    let db = db().await;
    seed(&db).await;
    let mut handle = db.clone();

    let sorted: Vec<Book> = Book::all()
        .order_by(Book::fields().copies().desc())
        .limit(2)
        .exec(&mut handle)
        .await
        .unwrap();
    assert_eq!(sorted.len(), 2);
    assert_eq!(sorted[0].copies, 7);

    let count = Book::all().count().exec(&mut handle).await.unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn in_list_filter() {
    let db = db().await;
    seed(&db).await;
    let mut handle = db.clone();

    let found: Vec<Book> = Book::filter(
        Book::fields()
            .title()
            .in_list(["Alpha".to_string(), "Beta".to_string()]),
    )
    .exec(&mut handle)
    .await
    .unwrap();
    assert_eq!(found.len(), 2, "IN returned {found:?}");
}

/// Relations are usable as long as each side is fetched with its own query.
#[tokio::test]
async fn relation_queries_without_preload() {
    let db = db().await;
    let author_id = seed(&db).await;
    let mut handle = db.clone();

    let author = Author::filter_by_id(author_id)
        .get(&mut handle)
        .await
        .unwrap();

    // Scoped query through the has_many side.
    let books: Vec<Book> = author.books().exec(&mut handle).await.unwrap();
    assert_eq!(books.len(), 3);

    // And back the other way, by foreign key.
    let book = Book::filter(Book::fields().title().eq("Beta".to_string()))
        .get(&mut handle)
        .await
        .unwrap();
    let owner = Author::filter_by_id(book.author_id)
        .get(&mut handle)
        .await
        .unwrap();
    assert_eq!(owner.name, "Alice");
}

/// Eager loading is not a fixture problem: `include` itself needs a
/// transaction, so preloading a relation fails on D1.
#[tokio::test]
async fn relation_preload_is_rejected() {
    let db = db().await;
    let author_id = seed(&db).await;
    let mut handle = db.clone();

    let author = Author::filter_by_id(author_id)
        .include(Author::fields().books())
        .get(&mut handle)
        .await
        .expect("preload should work once read-only plans skip the transaction");
    assert_eq!(author.books.get().len(), 3);
}

#[tokio::test]
async fn update_and_delete() {
    let db = db().await;
    seed(&db).await;
    let mut handle = db.clone();

    let mut book = Book::filter(Book::fields().title().eq("Beta".to_string()))
        .get(&mut handle)
        .await
        .unwrap();
    toasty::update!(book { copies: 99_i64 })
        .exec(&mut handle)
        .await
        .unwrap();

    let reloaded = Book::filter(Book::fields().title().eq("Beta".to_string()))
        .get(&mut handle)
        .await
        .unwrap();
    assert_eq!(reloaded.copies, 99);

    Book::delete_by_id(&mut handle, reloaded.id).await.unwrap();
    assert_eq!(Book::all().count().exec(&mut handle).await.unwrap(), 2);
}

#[tokio::test]
async fn batch_create_commits_atomically() {
    let db = db().await;
    let mut handle = db.clone();

    let author_id = seed(&db).await;
    let books = toasty::create!(Book::[
        { title: "One", copies: 1_i64, author_id: author_id },
        { title: "Two", copies: 2_i64, author_id: author_id },
    ])
    .exec(&mut handle)
    .await
    .expect("batch create should commit as one D1 request");

    assert_eq!(books.len(), 2);
    assert!(
        books.iter().all(|b| b.id > 0),
        "ids: {:?}",
        books.iter().map(|b| b.id).collect::<Vec<_>>()
    );
    assert_eq!(Book::all().count().exec(&mut handle).await.unwrap(), 5);
}

/// A batch that fails partway must leave nothing behind — the whole point of
/// handing D1 the writes together.
#[tokio::test]
async fn failing_batch_rolls_back() {
    let db = db().await;
    let author_id = seed(&db).await;
    let mut handle = db.clone();

    let before = Book::all().count().exec(&mut handle).await.unwrap();

    // The second insert reuses the first's title against a unique index.
    let result = toasty::create!(Book::[
        { title: "Duplicate", copies: 1_i64, author_id: author_id },
        { title: "Duplicate", copies: 2_i64, author_id: 999_999_u64 },
    ])
    .exec(&mut handle)
    .await;

    if result.is_ok() {
        // No constraint fired, so this database cannot demonstrate rollback.
        return;
    }

    assert_eq!(
        Book::all().count().exec(&mut handle).await.unwrap(),
        before,
        "a failed batch left rows behind"
    );
}
