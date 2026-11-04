// Mouse, wheel/trackpad and touch input on the desktop surface.
import { cmd } from '../protocol';
import type { App } from './app';
import { createTouch } from './touch';

/** DOM button -> X button (left, middle, right, back, forward). */
const BUTTONS = [1, 2, 3, 8, 9];

export function setupPointer(app: App, onInteract: () => void) {
	const { surface, send } = app;

	let rect = surface.getBoundingClientRect();
	const refreshRect = () => {
		rect = surface.getBoundingClientRect();
	};
	window.addEventListener('resize', refreshRect);
	function toDesk(p: { clientX: number; clientY: number }): [number, number] {
		const x = Math.round(((p.clientX - rect.left) * app.desk.w) / (rect.width || 1));
		const y = Math.round(((p.clientY - rect.top) * app.desk.h) / (rect.height || 1));
		return [
			Math.max(0, Math.min(app.desk.w - 1, x)),
			Math.max(0, Math.min(app.desk.h - 1, y)),
		];
	}
	let lastMove = '';
	let pendingMove: { clientX: number; clientY: number } | null = null;
	let moveQueued = false;
	function sendMove(p: { clientX: number; clientY: number }, force = false) {
		if (!app.desk.w) return;
		const s = cmd.move(...toDesk(p));
		if (s !== lastMove || force) {
			lastMove = s;
			send(s);
		}
	}
	function flushMove() {
		moveQueued = false;
		const p = pendingMove;
		pendingMove = null;
		if (!p) return;
		refreshRect();
		sendMove(p);
	}
	/** Last position in a frame wins. Buttons flush the pending move first. */
	function move(p: { clientX: number; clientY: number }, force = false) {
		if (force) {
			pendingMove = null;
			moveQueued = false;
			refreshRect();
			sendMove(p, true);
			return;
		}
		pendingMove = p;
		if (moveQueued) return;
		moveQueued = true;
		requestAnimationFrame(flushMove);
	}

	// Scroll is sent in desktop pixels, batched per frame; the server turns it into
	// smooth (pixel-precise) scrolling, or wheel clicks on X servers that can't.
	let sx = 0,
		sy = 0,
		queued = false;
	function queueScroll(dx: number, dy: number) {
		sx += dx;
		sy += dy;
		if (queued) return;
		queued = true;
		requestAnimationFrame(() => {
			queued = false;
			if (Math.abs(sx) < 0.5 && Math.abs(sy) < 0.5) return;
			send(cmd.scroll(sx, sy));
			sx = 0;
			sy = 0;
		});
	}
	/** Desktop pixels per on-screen pixel. */
	const deskPerPx = () => {
		refreshRect();
		return app.desk.w / (rect.width || 1);
	};

	const touch = createTouch(app, {
		moveTo: (x, y) => move({ clientX: x, clientY: y }, true),
		queueScroll,
		deskPerPx,
	});

	surface.addEventListener('pointermove', (e) => {
		if (e.pointerType === 'touch') touch.move(e);
		else move(e);
	});
	surface.addEventListener('pointerdown', (e) => {
		e.preventDefault();
		onInteract();
		if (e.pointerType === 'touch') {
			touch.down(e);
			return;
		}
		app.kbd.focus({ preventScroll: true });
		move(e);
		try {
			surface.setPointerCapture(e.pointerId);
		} catch {}
		send(cmd.button(BUTTONS[e.button] || 1, true));
	});
	surface.addEventListener('pointerup', (e) => {
		e.preventDefault();
		if (e.pointerType === 'touch') {
			touch.up(e);
			return;
		}
		move(e);
		send(cmd.button(BUTTONS[e.button] || 1, false));
	});
	surface.addEventListener('pointercancel', (e) => {
		if (e.pointerType === 'touch') touch.cancel(e);
		else send(cmd.releaseAll);
	});
	surface.addEventListener('contextmenu', (e) => e.preventDefault());
	surface.addEventListener(
		'wheel',
		(e) => {
			e.preventDefault();
			move(e);
			const k =
				(e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1) * deskPerPx();
			queueScroll(e.deltaX * k, e.deltaY * k);
		},
		{ passive: false }
	);
}
