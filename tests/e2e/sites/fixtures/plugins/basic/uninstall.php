<?php
/**
 * Uninstall routine: removes every option the plugin created.
 *
 * @package ApvmFixtures
 */

defined( 'WP_UNINSTALL_PLUGIN' ) || exit;

delete_option( 'apvm_fx_basic_activations' );
delete_option( 'apvm_fx_basic_deactivations' );
