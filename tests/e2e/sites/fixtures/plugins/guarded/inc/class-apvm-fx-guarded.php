<?php
/**
 * Shared implementation of the guarded family. Both editions declare the same class,
 * which is why the main files guard on it (like BackWPup's \BackWPup class).
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

/**
 * Wires one edition the way BackWPup 5.7 does (src/Plugin/Plugin.php): there is no
 * activation hook. On every request in which this edition is the loaded one, its install
 * routine runs on `init` when the stored version differs from its own. Events are logged
 * in the `apvm_fx_guarded_log` option for the E2E checks.
 */
final class Apvm_Fx_Guarded {

	/**
	 * Registers the deactivation hook and, when needed, the install routine.
	 *
	 * @param string $file    Main plugin file of the edition.
	 * @param string $edition Edition name, `free` or `pro`.
	 * @return void
	 */
	public static function boot( string $file, string $edition ): void {
		$version = get_file_data( $file, array( 'Version' => 'Version' ) )['Version'];
		register_deactivation_hook( $file, static fn() => self::log( 'deactivated:' . $edition ) );
		if ( get_site_option( 'apvm_fx_guarded_version' ) !== $version ) {
			add_action( 'init', static fn() => self::install( $edition, $version ), 0 );
		}
	}

	/**
	 * The install/upgrade routine: records the version and writes the shared data.
	 *
	 * @param string $edition Edition name.
	 * @param string $version Edition version.
	 * @return void
	 */
	private static function install( string $edition, string $version ): void {
		update_site_option( 'apvm_fx_guarded_version', $version );
		update_option( 'apvm_fx_guarded_shared', 'written-by-' . $edition );
		self::log( 'installed:' . $edition . '@' . $version );
	}

	/**
	 * Appends an event to the family log option.
	 *
	 * @param string $event Event label.
	 * @return void
	 */
	private static function log( string $event ): void {
		$log   = (array) get_option( 'apvm_fx_guarded_log', array() );
		$log[] = $event;
		update_option( 'apvm_fx_guarded_log', $log );
	}
}
