// Presenting decoded frames and fitting the desktop to the window.
import type { Stripe, StripeFrame } from '../protocol';
import type { App } from './app';

interface StripeOut {
	video: HTMLVideoElement;
	writer: WritableStreamDefaultWriter<VideoFrame>;
	writing: boolean;
	next: VideoFrame | null;
	onDone?: () => void;
}

/**
 * Prefer handing decoded frames to a <video> via MediaStreamTrackGenerator: the
 * compositor scans YUV out directly instead of rasterizing into a 2D canvas. One
 * <video> per encoded stripe, stacked; the worker hands over every stripe of a frame
 * in one message so they are written to their tracks together.
 */
// At most one write in flight per track; a newer frame replaces a waiting one.
// `onDone` runs when this frame's write has settled, or when a newer one takes its place in the queue.
function write(o: StripeOut, frame: VideoFrame, onDone: () => void) {
	if (o.writing) {
		if (o.next) {
			o.next.close();
			o.onDone?.();
		}
		o.next = frame;
		o.onDone = onDone;
		return;
	}
	o.writing = true;
	o.writer
		.write(frame)
		.catch(() => {})
		.finally(() => {
			o.writing = false;
			const next = o.next;
			const nextDone = o.onDone;
			o.next = null;
			o.onDone = undefined;
			onDone();
			if (next && nextDone) write(o, next, nextDone);
		});
}

const gcd = (a: number, b: number): number => (b ? gcd(b, a % b) : a);

export function createVideo(
	app: App,
	canvas: HTMLCanvasElement,
	useVideo: boolean,
	onLayout: () => void,
	onView?: (w: number, h: number) => void
) {
	let outs: StripeOut[] = [];
	let stripes: Stripe[] = [[0, 1]];
	if (useVideo) canvas.style.display = 'none';

	function build(list: Stripe[]) {
		stripes = list;
		for (const o of outs) o.video.remove();
		outs = [];
		if (!useVideo) return;
		for (const [y, h] of list) {
			const gen = new MediaStreamTrackGenerator({ kind: 'video' });
			const video = document.createElement('video');
			video.className = 'stripe';
			video.muted = true;
			video.autoplay = true;
			video.playsInline = true;
			video.disablePictureInPicture = true;
			video.style.top = `${(y / app.desk.h) * 100}%`;
			video.style.height = `${(h / app.desk.h) * 100}%`;
			video.srcObject = new MediaStream([gen]);
			app.surface.appendChild(video);
			video.play().catch(() => {});
			outs.push({
				video,
				writer: gen.writable.getWriter(),
				writing: false,
				next: null,
			});
		}
	}

	function present(id: number, parts: StripeFrame[], onDone: (id: number) => void) {
		if (!useVideo || !outs.length) {
			for (const p of parts) p.frame.close();
			onDone(id);
			return;
		}
		let left = 0;
		let finished = false;
		const done = () => {
			if (finished) return;
			left--;
			if (left <= 0) {
				finished = true;
				onDone(id);
			}
		};
		for (const p of parts) {
			const o = outs[p.i];
			if (!o) {
				p.frame.close();
				continue;
			}
			left++;
			write(o, p.frame, done);
		}
		if (left === 0) onDone(id);
	}

	function layout() {
		const { w: dw, h: dh } = app.desk;
		if (!dw) return;
		const vw = window.innerWidth,
			vh = window.innerHeight;
		const dpr = window.devicePixelRatio || 1;
		// Snap the height so every stripe boundary lands on a whole device pixel;
		// otherwise the stacked stripe layers show hairline seams.
		const q = dh / stripes.reduce((g, [y]) => gcd(g, y), dh);
		const devH = Math.max(
			q,
			Math.floor(((Math.min(vw / dw, vh / dh) || 1) * dh * dpr) / q) * q
		);
		app.view.scale = devH / dpr / dh;
		app.surface.style.width = `${dw * app.view.scale}px`;
		app.surface.style.height = `${dh * app.view.scale}px`;
		onView?.(Math.max(1, Math.round((dw * devH) / dh)), devH);
		onLayout();
	}

	return { build, present, layout };
}
