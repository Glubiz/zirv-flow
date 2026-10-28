# Claim-to-test map

Each enforced claim (E1 to E20) of the [compliance guide](compliance.md) maps to the tests that prove it. It is a table, not inline references, so one check can resolve every name once and the guide stays readable. A test in the suite fails when an id of the guide has no row here, when a row names an id the guide lacks, or when a named test function does not exist in the named file under `crates/`.

| Id | Claim | Test | File |
| --- | --- | --- | --- |
| E1 | Publish is admin only and each publish is a decision | `the_policy_draft_is_written_by_writers_published_by_admins_and_each_publish_is_a_definition_change_decision` | `crates/zirv-flow-api/src/acceptance.rs` |
| E1 | A rule that no longer compiles denies the send | `a_rule_that_can_no_longer_be_compiled_denies_the_send_and_is_named` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E1 | A published version is immutable | `a_published_version_is_immutable_and_each_publish_is_a_definition_change_decision` | `crates/zirv-flow-store/src/consent/policy.rs` |
| E1 | The built-in rule cannot be removed | `an_invalid_policy_is_refused_with_every_issue_and_the_built_in_rule_cannot_be_removed` | `crates/zirv-flow-api/src/acceptance.rs` |
| E1 | A rule denies a send and the skip names it | `a_policy_of_recorded_consent_a_cooldown_and_an_account_state_decides_sends_and_each_skip_names_its_rule` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E1 | A transactional send skips the policy | `a_transactional_send_skips_the_policy_with_consent` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E2 | A channel-wide unsubscribe blocks every topic | `a_channel_wide_unsubscribe_blocks_every_topic` | `crates/zirv-flow-core/src/consent.rs` |
| E2 | Opt-in and opt-out topics | `opt_in_topics_need_a_subscription_and_opt_out_topics_only_lack_an_unsubscribe` | `crates/zirv-flow-core/src/consent.rs` |
| E2 | Evidence is at most 4 KiB | `evidence_is_an_object_of_at_most_four_kib_and_defaults_to_empty` | `crates/zirv-flow-api/src/consent/person.rs` |
| E2 | A repeat leaves no history | `writing_the_status_already_held_leaves_no_history_and_no_decision` | `crates/zirv-flow-store/src/consent/state.rs` |
| E2 | History cannot be updated | `the_history_cannot_be_updated_but_can_be_deleted_for_erasure` | `crates/zirv-flow-store/src/consent/state.rs` |
| E2 | Unsubscribed wins a tie on merge | `unsubscribed_wins_a_tie_and_the_merge_is_in_the_survivors_history` | `crates/zirv-flow-store/src/consent/merge.rs` |
| E3 | A GET never unsubscribes, the POST does once | `a_get_never_unsubscribes_and_the_form_and_the_one_click_post_do_once` | `crates/zirv-flow-api/src/tracking/mod.rs` |
| E3 | A preference change blocks only its vertical | `the_preference_center_saves_a_vertical_change_that_blocks_only_that_vertical` | `crates/zirv-flow-api/src/tracking/mod.rs` |
| E3 | A bad token is a not-found page | `a_bad_token_is_the_404_page_never_the_web_app` | `crates/zirv-flow-api/src/tracking/mod.rs` |
| E3 | A removed scope falls back to the channel | `a_one_click_post_for_a_removed_dimension_value_denies_the_channel` | `crates/zirv-flow-api/src/tracking/mod.rs` |
| E3 | The next send is skipped | `after_an_unsubscribe_the_next_send_is_skipped_for_no_consent_and_the_decision_keeps_the_trace` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E4 | The first reason is kept, erased replaces | `re_adding_keeps_the_first_reason_but_erased_overwrites_and_stays` | `crates/zirv-flow-store/src/suppressions.rs` |
| E4 | An erased suppression cannot be removed | `an_erased_suppression_cannot_be_removed_but_others_can` | `crates/zirv-flow-store/src/suppressions.rs` |
| E4 | Organization and workspace scope | `an_organization_suppression_blocks_every_workspace_and_a_workspace_one_only_its_own` | `crates/zirv-flow-store/src/suppressions.rs` |
| E4 | A suppression outlives the person | `a_suppressed_identifier_stays_suppressed_by_its_hash_after_the_person_is_gone` | `crates/zirv-flow-store/src/suppressions.rs` |
| E4 | A list shows a prefix only | `a_listed_suppression_shows_a_four_byte_prefix_and_never_the_whole_hash` | `crates/zirv-flow-api/src/consent/suppressions.rs` |
| E4 | A hard bounce suppresses organization-wide | `a_hard_bounce_ends_the_message_as_email_bounced_and_suppresses_the_address_in_the_organization` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E5 | Every gate passes and is recorded | `a_message_passes_every_gate_and_is_sent_once_with_its_log_event_and_decisions` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E5 | Each gate stops with its own reason | `every_gate_stops_a_message_with_its_own_reason_and_stores_it_exactly` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E5 | Transactional skips consent, not suppression | `a_transactional_send_skips_consent_but_not_suppression` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E5 | The gates are in the trace | `the_send_path_and_its_trace_hold_end_to_end` | `crates/zirv-flow/src/e2e/mod.rs` |
| E6 | Quiet hours hold a send | `a_send_in_quiet_hours_waits_for_their_end_releases_its_slot_and_is_sent_once` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E6 | Transactional is not held | `a_transactional_send_is_not_held_by_quiet_hours` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E6 | A defer cap holds a message | `a_cap_in_defer_mode_holds_the_second_message_until_the_first_leaves_the_window` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E6 | A skip cap skips, transactional exempt | `a_cap_in_skip_mode_skips_the_second_message_and_a_transactional_one_is_exempt` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E6 | Concurrent flows never exceed a cap | `sixteen_flows_sending_to_one_person_at_once_never_exceed_a_cap_of_one_in_skip_mode` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E6 | Only admins replace the settings | `send_settings_are_read_by_readers_and_replaced_only_by_admins` | `crates/zirv-flow-api/src/sending/acceptance.rs` |
| E7 | An unverified domain is skipped | `a_send_from_an_unverified_domain_is_skipped_before_any_connection` | `crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs` |
| E7 | A domain verifies only with all three records | `a_domain_stays_unverified_until_spf_dkim_and_dmarc_all_resolve_and_drops_back` | `crates/zirv-flow-jobs/src/kinds/sending_domain_verify.rs` |
| E8 | Every data class goes, only the ledger stays | `erasing_a_person_with_every_data_class_leaves_only_the_ledger` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E8 | A failure rolls everything back | `a_failure_midway_rolls_everything_back_and_leaves_the_request_pending` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E8 | A new person stays suppressed | `a_pending_row_cannot_recreate_the_person_and_the_new_person_stays_suppressed` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E8 | Transcripts are deleted by substring within the workspace | `erasure_deletes_only_the_transcripts_that_name_the_person_in_that_workspace` | `crates/zirv-flow-store/src/ai/transcripts.rs` |
| E8 | No table holds a value outside the allowlist | `no_table_holds_a_value_of_an_erased_person_outside_the_allowlist` | `crates/zirv-flow-store/src/privacy/content_scan.rs` |
| E8 | Every table is classified | `every_table_of_the_schema_is_classified` | `crates/zirv-flow-store/src/privacy/content_scan.rs` |
| E8 | The job erases and completes the request | `the_job_erases_the_person_suppresses_the_email_and_completes_the_request` | `crates/zirv-flow-jobs/src/kinds/privacy_erase.rs` |
| E8 | Limit: matching is by whole field | `a_needle_matches_only_as_a_whole_field` | `crates/zirv-flow-store/src/by_value.rs` |
| E8 | Transcripts of the person are deleted | `erasing_a_person_deletes_the_ai_transcripts_that_name_them_and_no_others` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E9 | Merged people are erased with the survivor | `erasing_a_person_with_every_data_class_leaves_only_the_ledger` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E9 | A backup before a merge is erased too | `replay_erases_every_id_the_group_had_when_a_backup_predates_the_merge` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E10 | Only declaring destinations get one call | `only_the_destinations_that_declare_erase_receive_one_call_with_a_stable_key` | `crates/zirv-flow/src/e2e/privacy.rs` |
| E10 | A failing destination blocks nothing | `a_retryable_then_a_terminal_failure_is_recorded_per_destination_and_blocks_nothing` | `crates/zirv-flow/src/e2e/privacy.rs` |
| E10 | SMTP and webhook declare no erase | `a_destination_body_has_the_capabilities_of_its_type` | `crates/zirv-flow-api/src/destinations/crud.rs` |
| E11 | The file is written before the job returns | `the_job_writes_the_ledger_file_before_it_returns` | `crates/zirv-flow-jobs/src/kinds/privacy_erase.rs` |
| E11 | A failed write fails the job | `an_unwritable_directory_fails_the_job_and_leaves_the_request_pending` | `crates/zirv-flow-jobs/src/kinds/privacy_erase.rs` |
| E11 | A file is atomic and leaves no temp file | `a_written_file_reads_back_and_leaves_no_temp_file` | `crates/zirv-flow-store/src/privacy/ledger_files.rs` |
| E12 | An erased person stays erased after a restore | `an_erased_person_stays_erased_after_the_restore_and_the_replay_applies_a_later_erasure` | `crates/zirv-flow/src/e2e/restore.rs` |
| E12 | A second replay changes nothing | `a_second_replay_changes_nothing_and_re_creates_nothing` | `crates/zirv-flow/src/e2e/restore.rs` |
| E12 | Suppressions come back | `replay_erases_a_restored_person_again_and_restores_the_suppressions` | `crates/zirv-flow-store/src/privacy/erasure.rs` |
| E13 | Every section of the person | `an_export_holds_every_section_of_the_person_and_the_merged_people` | `crates/zirv-flow-store/src/privacy/export.rs` |
| E13 | Every person table is exported or excluded | `every_table_with_a_person_column_is_exported_or_excluded_with_a_reason` | `crates/zirv-flow-store/src/privacy/export.rs` |
| E13 | The size cap | `a_person_over_the_cap_fails_with_export_too_large_and_leaves_no_content` | `crates/zirv-flow-store/src/privacy/export.rs` |
| E13 | The link expires | `an_export_past_its_time_is_gone_even_before_the_upkeep_and_the_upkeep_drops_it` | `crates/zirv-flow-store/src/privacy/export.rs` |
| E14 | The windows and who may edit them | `get_lists_all_eight_windows_with_sources_bounds_and_editability` | `crates/zirv-flow-api/src/retention/acceptance.rs` |
| E14 | A workspace window stays within bounds | `put_refuses_longer_than_the_deployment_below_the_minimum_and_unknown_fields` | `crates/zirv-flow-api/src/retention/acceptance.rs` |
| E14 | Only admins change a window | `only_admins_and_the_owner_may_read_or_change_the_windows` | `crates/zirv-flow-api/src/retention/acceptance.rs` |
| E14 | A change is audited without values | `the_audit_row_holds_window_names_and_numbers_only` | `crates/zirv-flow-api/src/retention/acceptance.rs` |
| E14 | Decision-log defaults and bounds | `the_decision_retention_defaults_are_13_months_and_30_days_to_25_months` | `crates/zirv-flow/src/config.rs` |
| E14 | Decision-log bounds are enforced | `the_decision_retention_refuses_29_days_and_26_months_and_unordered_bounds` | `crates/zirv-flow/src/config.rs` |
| E14 | Event defaults and overrides | `the_event_retention_defaults_are_25_months_and_30_to_1095_day_overrides` | `crates/zirv-flow/src/config.rs` |
| E14 | Message defaults | `the_message_retention_defaults_are_13_months_and_30_to_1095_day_windows` | `crates/zirv-flow/src/config.rs` |
| E14 | Audit and job-history bounds | `the_job_history_and_audit_windows_are_refused_outside_their_bounds` | `crates/zirv-flow/src/config.rs` |
| E14 | The other deployment windows | `the_former_constants_are_refused_outside_their_bounds` | `crates/zirv-flow/src/config.rs` |
| E14 | A workspace window deletes its own rows | `a_workspace_window_deletes_that_workspaces_messages_only` | `crates/zirv-flow-jobs/src/kinds/messages_retention.rs` |
| E14 | The decision log deletes by window | `a_workspace_window_deletes_that_workspaces_records_only` | `crates/zirv-flow-jobs/src/kinds/decisions_retention.rs` |
| E14 | Job history | `a_scheduled_run_removes_dead_jobs_past_14_days_and_keeps_the_rest` | `crates/zirv-flow-jobs/src/kinds/jobs_retention.rs` |
| E15 | Who sees values | `the_truth_table_of_who_sees_personal_values` | `crates/zirv-flow-api/src/auth.rs` |
| E15 | The pii scope alone reads nothing | `the_pii_scope_alone_reads_nothing_and_adds_nothing_to_another_scope` | `crates/zirv-flow-api/src/auth.rs` |
| E15 | The view_pii grant unmasks the profile | `the_view_pii_grant_unmasks_the_profile_and_timeline_for_a_viewer_and_is_kept_when_omitted` | `crates/zirv-flow-api/src/acceptance.rs` |
| E15 | A read key masks | `a_pii_key_needs_read_to_read_and_read_alone_masks` | `crates/zirv-flow-api/src/acceptance.rs` |
| E15 | The debugger masks per viewer | `the_debugger_streams_outcomes_in_order_masked_per_viewer_and_never_unredacted` | `crates/zirv-flow-api/src/acceptance.rs` |
| E15 | A reject sample follows the strict rule | `a_rejected_event_follows_the_stricter_sample_rule_unless_revealed` | `crates/zirv-flow-api/src/events/debugger.rs` |
| E15 | The explorer refuses probes | `the_explorer_refuses_pii_probes_for_members_and_bad_input_for_everyone` | `crates/zirv-flow-api/src/acceptance.rs` |
| E15 | An editor needs the grant for a real person | `an_editor_needs_the_view_pii_grant_for_a_real_person_preview` | `crates/zirv-flow-api/src/templates/acceptance.rs` |
| E16 | The log is append-only | `update_delete_and_truncate_are_refused` | `crates/zirv-flow-store/src/audit.rs` |
| E16 | A summary with personal data is refused | `a_summary_with_personal_data_is_refused_and_nothing_is_written` | `crates/zirv-flow-store/src/audit.rs` |
| E16 | Every change leaves one row | `every_audited_route_leaves_one_row_and_every_not_audited_route_none` | `crates/zirv-flow-api/src/rbac/audit_acceptance.rs` |
| E16 | Workspace rows are for admins | `the_workspace_audit_routes_are_for_admins_and_the_owner` | `crates/zirv-flow-api/src/audit/acceptance.rs` |
| E16 | Instance rows are for the owner | `the_instance_audit_routes_are_for_the_owner_only` | `crates/zirv-flow-api/src/audit/acceptance.rs` |
| E16 | An export is audited | `both_export_formats_stream_and_are_audited` | `crates/zirv-flow-api/src/audit/acceptance.rs` |
| E16 | Retention purges and records it | `a_scheduled_run_purges_old_rows_and_records_one_purge_row` | `crates/zirv-flow-jobs/src/kinds/audit_retention.rs` |
| E14 | A run that deletes nothing writes no purge row | `a_run_that_deletes_nothing_writes_no_purge_row` | `crates/zirv-flow-jobs/src/kinds/audit_retention.rs` |
| E17 | No table holds a stored secret in the clear | `no_table_holds_the_plaintext_of_a_stored_secret` | `crates/zirv-flow-store/src/secrets.rs` |
| E17 | A credential is never returned or logged | `a_credential_is_sealed_with_its_own_purpose_and_never_returned_logged_or_audited` | `crates/zirv-flow-api/src/destinations/acceptance.rs` |
| E17 | The provider key is never echoed | `enabling_probes_the_model_and_the_key_is_never_echoed_logged_or_stored_in_plaintext` | `crates/zirv-flow-api/src/acceptance.rs` |
| E18 | Metadata needs the exact address | `a_broad_range_never_unlocks_metadata_but_the_exact_address_does` | `crates/zirv-flow-core/src/egress.rs` |
| E18 | A mixed answer is refused whole | `a_mixed_set_is_denied_as_a_whole` | `crates/zirv-flow-core/src/egress.rs` |
| E18 | Loopback is never contacted | `a_host_resolving_to_loopback_is_blocked_by_default_and_never_contacted` | `crates/zirv-flow-connectors/src/http/client.rs` |
| E18 | A redirect to a blocked address is refused | `a_redirect_to_a_blocked_literal_is_refused` | `crates/zirv-flow-connectors/src/http/client.rs` |
| E18 | Proxy variables are ignored | `proxy_environment_variables_are_not_used` | `crates/zirv-flow-connectors/src/http/client.rs` |
| E18 | The assistant client is guarded | `a_host_resolving_to_loopback_is_blocked_until_its_cidr_is_allowlisted` | `crates/zirv-flow-ai/src/http.rs` |
| E19 | Default-level logs hold no identifier | `default_level_logs_hold_no_identifier_value_through_the_whole_privacy_scenario` | `crates/zirv-flow/src/e2e/privacy.rs` |
| E20 | Personal content is refused without the opt-in | `personal_content_is_refused_everywhere_without_the_opt_in` | `crates/zirv-flow-ai/src/request.rs` |
| E20 | No person reaches the provider | `the_model_context_and_the_response_hold_no_person_even_with_the_pii_opt_in` | `crates/zirv-flow-api/src/assistant/acceptance.rs` |
| E20 | At most five sample records go out, within an 8 KiB total | `sample_records_are_capped_by_count_and_bytes` | `crates/zirv-flow-api/src/assistant/prompt.rs` |
| E20 | A record that would pass the byte cap ends the list and the earlier ones stay | `sample_records_stop_at_the_first_that_passes_the_byte_cap` | `crates/zirv-flow-api/src/assistant/prompt.rs` |
| E20 | Without the opt-in the context has no personal part | `schema_only_context_has_no_personal_part` | `crates/zirv-flow-api/src/assistant/prompt.rs` |
| E20 | With the opt-in the sample values are sent fenced | `with_the_opt_in_the_sample_values_are_sent_fenced` | `crates/zirv-flow-api/src/assistant/acceptance.rs` |
| E20 | Webhook URLs are masked under the opt-in | `webhook_urls_are_masked_under_the_opt_in_too_and_a_placeholder_stays_with_its_node` | `crates/zirv-flow-api/src/assistant/acceptance.rs` |
| E20 | Transcripts only when opted in | `transcripts_are_stored_only_when_opted_in_and_expire_after_the_window` | `crates/zirv-flow-api/src/acceptance.rs` |
| E20 | Expired transcripts are purged | `the_job_purges_only_expired_transcripts_and_a_rerun_changes_nothing` | `crates/zirv-flow-jobs/src/kinds/ai_transcripts.rs` |
