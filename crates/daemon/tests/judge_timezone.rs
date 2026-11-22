//! 当地日的边界在隔离的测试进程里验证；不会修改其他测试的进程时区。

#[cfg(unix)]
#[test]
fn local_midnight_handles_dst_and_non_utc_offsets() {
    // 这里只有一个测试，TZ 在第一次调用时间 API 之前设置。
    std::env::set_var("TZ", "America/New_York");
    use anycon_daemon::judge::service::local_day_start;
    // 2024-03-10 12:00 UTC：当天零点仍是 UTC-5，下午已经是 UTC-4。
    assert_eq!(local_day_start(1_710_072_000_000), Some(1_710_046_800_000));
    // 2024-11-03 12:00 UTC：当天零点仍是 UTC-4，下午已经是 UTC-5。
    assert_eq!(local_day_start(1_730_635_200_000), Some(1_730_606_400_000));
    // UTC 的午夜还在本地前一天。
    assert_eq!(local_day_start(1_710_028_800_000), Some(1_709_960_400_000));
}
