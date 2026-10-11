<?php
/**
 * Uninstall routine: removes what activation wrote into wp-content.
 *
 * @package ApvmFixtures
 */

defined( 'WP_UNINSTALL_PLUGIN' ) || exit;

$apvm_fx_dir = WP_CONTENT_DIR . '/apvm-fx-writes-content';
wp_delete_file( $apvm_fx_dir . '/state.txt' );
if ( is_dir( $apvm_fx_dir ) ) {
	rmdir( $apvm_fx_dir ); // phpcs:ignore WordPress.WP.AlternativeFunctions
}
