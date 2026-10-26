//! Makes the `browser-test` feature impossible to compile by accident: `--all-features`, an alias
//! or a default list all fail here unless the builder deliberately sets the variable.

const _: () = assert!(
    option_env!("ZIRV_FLOW_BROWSER_TEST_BUILD").is_some(),
    "browser-test is for the browser test server only: set ZIRV_FLOW_BROWSER_TEST_BUILD=1 to build it",
);
