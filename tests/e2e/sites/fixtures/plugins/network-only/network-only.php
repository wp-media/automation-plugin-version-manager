<?php
/**
 * Plugin Name: APVM Fixture Network Only
 * Description: E2E fixture. A network-only plugin (Network: true) that counts activations.
 * Version: 1.0.0
 * Network: true
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

register_activation_hook(
	__FILE__,
	static fn() => update_site_option( 'apvm_fx_network_only_activations', (int) get_site_option( 'apvm_fx_network_only_activations', 0 ) + 1 )
);
