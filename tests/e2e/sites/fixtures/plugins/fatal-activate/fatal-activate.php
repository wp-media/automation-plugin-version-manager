<?php
/**
 * Plugin Name: APVM Fixture Fatal Activate
 * Description: E2E fixture. Its activation hook throws, so activation always fails with a PHP fatal error.
 * Version: 1.0.0
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

register_activation_hook(
	__FILE__,
	static function () {
		throw new RuntimeException( 'APVM fixture: activation fails on purpose.' );
	}
);
