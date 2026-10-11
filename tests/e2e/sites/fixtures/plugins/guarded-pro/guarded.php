<?php
/**
 * Plugin Name: APVM Fixture Guarded Pro
 * Description: E2E fixture mirroring BackWPup's free/pro family: a shared main-file guard and shared data.
 * Version: 1.0.0
 * Requires at least: 6.0
 * Requires PHP: 7.4
 * Network: true
 * License: GPL-2.0-or-later
 *
 * @package ApvmFixtures
 */

defined( 'ABSPATH' ) || exit;

// Mirrors backwpup.php line 25: with two family members active, the first one loaded
// wins and the other returns here, before registering any hook or install routine.
if ( defined( 'APVM_FX_GUARDED_LOADED' ) || class_exists( \Apvm_Fx_Guarded::class, false ) ) {
	return;
}

define( 'APVM_FX_GUARDED_LOADED', 'pro' );

require_once __DIR__ . '/inc/class-apvm-fx-guarded.php';

Apvm_Fx_Guarded::boot( __FILE__, 'pro' );
