<?php
/**
 * Uninstall routine: disarms the plugin.
 *
 * @package ApvmFixtures
 */

defined( 'WP_UNINSTALL_PLUGIN' ) || exit;

delete_option( 'apvm_fx_fatal_load_armed' );
