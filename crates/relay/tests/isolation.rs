//! 测试隔离回归（task-91）：唯一临时库 + 夹具 Drop 清理 + 已初始化库明确报错。

mod support;

use support::*;

#[test]
fn temp_paths_are_unique_even_with_same_label() {
    let first = temp_db_path("same-label");
    let second = temp_db_path("same-label");
    assert_ne!(first, second, "同 label 的两次调用也必须拿到不同库");
    cleanup_db_dir(&first);
    cleanup_db_dir(&second);
}

#[test]
fn fixtures_with_same_label_do_not_share_a_database() {
    // 两个夹具用同一个 label、同一进程并行时也必须各自初始化成功。
    let first = Fixture::new("collision");
    let second = Fixture::new("collision");
    assert_ne!(first.cfg.db.path, second.cfg.db.path);
    assert_eq!(first.vault().state, relay::vault::VaultState::Active);
    assert_eq!(second.vault().state, relay::vault::VaultState::Active);
    assert_eq!(first.vault().active_epoch, 1);
    assert_eq!(second.vault().active_epoch, 1);
}

#[test]
fn fixture_drop_removes_its_temp_database() {
    let path = {
        let fixture = Fixture::new("drop-cleanup");
        fixture.cfg.db.path.clone()
    };
    assert!(
        !path.parent().expect("db path has parent").exists(),
        "Fixture Drop 应删除临时目录"
    );
}

#[test]
fn into_db_path_retains_database_for_reopen() {
    let fixture = Fixture::new("retained");
    let path = fixture.into_db_path();
    assert!(path.exists(), "重开用例显式保留库文件");
    cleanup_db_dir(&path);
    assert!(!path.parent().expect("parent").exists());
}
