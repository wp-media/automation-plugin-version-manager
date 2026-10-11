<?php
/**
 * Uninstall routine mirroring BackWPup's: the shared data is removed only when no
 * family member is loaded in this request. The outcome is logged for the E2E checks.
 *
 * @package ApvmFixtures
 */

defined( 'WP_UNINSTALL_PLUGIN' ) || exit;

$apvm_fx_cleaned = ! class_exists( 'Apvm_Fx_Guarded' );
if ( $apvm_fx_cleaned ) {
	delete_option( 'apvm_fx_guarded_shared' );
	delete_site_option( 'apvm_fx_guarded_version' );
}

$apvm_fx_log   = (array) get_option( 'apvm_fx_guarded_log', array() );
$apvm_fx_log[] = ( $apvm_fx_cleaned ? 'uninstall-cleaned:' : 'uninstall-kept:' ) . 'pro';
update_option( 'apvm_fx_guarded_log', $apvm_fx_log );
