<?php
/**
 * Plugin Name: APVM Fixture Fatal Load
 * Description: E2E fixture. Activates cleanly, then breaks every request that loads it (health check, recovery).
 * Version: 1.0.0
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

// Armed by the activation hook, and only while active: the activation request itself
// succeeds, and so does a reactivation after a no-load (--skip-plugins) deactivation.
if (
	get_option( 'apvm_fx_fatal_load_armed' )
	&& in_array( plugin_basename( __FILE__ ), (array) get_option( 'active_plugins', array() ), true )
) {
	throw new RuntimeException( 'APVM fixture: this plugin breaks every request once activated.' );
}

register_activation_hook( __FILE__, static fn() => update_option( 'apvm_fx_fatal_load_armed', 1 ) );
register_deactivation_hook( __FILE__, static fn() => delete_option( 'apvm_fx_fatal_load_armed' ) );
