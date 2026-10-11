<?php
/**
 * Plugin Name: APVM Fixture Basic
 * Description: E2E fixture. Counts activations and deactivations in options; uninstall.php deletes them.
 * Version: 1.0.0
 * Requires at least: 6.0
 * Requires PHP: 7.4
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

/**
 * Increments an integer option, creating it at 1.
 *
 * @param string $name Option name.
 * @return void
 */
function apvm_fx_basic_bump( string $name ): void {
	update_option( $name, (int) get_option( $name, 0 ) + 1 );
}

register_activation_hook( __FILE__, static fn() => apvm_fx_basic_bump( 'apvm_fx_basic_activations' ) );
register_deactivation_hook( __FILE__, static fn() => apvm_fx_basic_bump( 'apvm_fx_basic_deactivations' ) );
