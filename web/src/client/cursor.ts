// The remote cursor, drawn locally by the browser so pointer movement has no stream
// latency. Cursor messages: type byte, pad, width, height, hot x, hot y (u16 each),
// serial (u32), then RGBA.
import type { App } from './app';

// On-screen cursor size bounds in CSS pixels, applied to the image's longer side.
// 24 is the standard pointer size (Xcursor's default), so a desktop scaled down
// into a small window (an embedded sidebar) keeps a normal-sized pointer.
const MIN_CURSOR_PX = 24;
const MAX_CURSOR_PX = 128;

export function createCursor(app: App) {
	let msg: ArrayBuffer | null = null;
	const scaled = document.createElement('canvas');
	let applied = '';
	let url: string | null = null;
	let gen = 0;

	function apply() {
		if (!msg) return;
		const v = new DataView(msg);
		const w = v.getUint16(2),
			h = v.getUint16(4),
			hx = v.getUint16(6),
			hy = v.getUint16(8),
			serial = v.byteLength >= 14 ? v.getUint32(10) : 0;
		if (!w || !h) {
			app.surface.style.cursor = 'none';
			return;
		}
		const dpr = window.devicePixelRatio || 1;
		const size = Math.max(w, h);
		const s = Math.min(
			Math.max(app.view.scale, MIN_CURSOR_PX / size),
			MAX_CURSOR_PX / size
		);
		const key = `${serial}:${s}:${dpr}`;
		if (key === applied && url) return;
		applied = key;
		const src = document.createElement('canvas');
		src.width = w;
		src.height = h;
		src
			.getContext('2d')!
			.putImageData(
				new ImageData(new Uint8ClampedArray(msg, 14, w * h * 4), w, h),
				0,
				0
			);
		// Cursor images are in desktop pixels; show them at the on-screen scale, but
		// never smaller than a normal pointer.
		const cw = Math.max(1, Math.round(w * s)),
			ch = Math.max(1, Math.round(h * s));
		const dpw = Math.max(1, Math.round(cw * dpr)),
			dph = Math.max(1, Math.round(ch * dpr));
		scaled.width = dpw;
		scaled.height = dph;
		const cx = scaled.getContext('2d')!;
		cx.imageSmoothingQuality = 'high';
		cx.clearRect(0, 0, dpw, dph);
		cx.drawImage(src, 0, 0, dpw, dph);
		const hsx = Math.round(hx * s),
			hsy = Math.round(hy * s);
		const ticket = ++gen;
		scaled.toBlob((blob) => {
			if (!blob || ticket !== gen) return;
			if (url) URL.revokeObjectURL(url);
			url = URL.createObjectURL(blob);
			app.surface.style.cursor =
				dpr > 1
					? `-webkit-image-set(url(${url}) ${dpr}x) ${hsx} ${hsy}, url(${url}) ${hsx} ${hsy}, default`
					: `url(${url}) ${hsx} ${hsy}, default`;
			if (dpr > 1 && !app.surface.style.cursor)
				app.surface.style.cursor = `url(${url}) ${hsx} ${hsy}, default`;
		}, 'image/png');
	}

	return {
		set(buf: ArrayBuffer) {
			msg = buf;
			apply();
		},
		apply,
	};
}
