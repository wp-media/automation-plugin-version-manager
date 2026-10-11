<?php
/**
 * Plugin Name: APVM Fixture Writes Content
 * Description: E2E fixture. Activation writes a directory and a file into wp-content (side-effect ownership).
 * Version: 1.0.0
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

register_activation_hook(
	__FILE__,
	static function () {
		$dir = WP_CONTENT_DIR . '/apvm-fx-writes-content';
		wp_mkdir_p( $dir );
		file_put_contents( $dir . '/state.txt', "activated\n" ); // phpcs:ignore WordPress.WP.AlternativeFunctions
	}
);
