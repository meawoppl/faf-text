/* tslint:disable */
/* eslint-disable */

export class FafTextDemo {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Create the renderer on a canvas. The canvas backing size should
     * already be set (CSS size × devicePixelRatio). Pass `force_gl` when a
     * JS-side `navigator.gpu.requestAdapter()` probe failed — wgpu's WebGPU
     * backend throws an uncatchable TypeError on null adapters.
     */
    static attach(canvas: HTMLCanvasElement, dpr: number, force_gl: boolean): Promise<FafTextDemo>;
    backend(): string;
    /**
     * Caret geometry in CSS pixels relative to the canvas: `[x, y, w, h]`.
     * JS parks the hidden IME input here so the composition popup lands on
     * the caret.
     *
     * While the pane is tilted the caret is projected through the same
     * matrices the glyphs go through, so the composition popup follows it
     * into 3D.
     */
    caret_css_rect(): Float32Array;
    /**
     * Composition finished: `commit` keeps the preedit text, otherwise it is
     * removed (the browser fires `compositionend` with the final text first,
     * so a committed run needs no further edit).
     */
    composition_end(commit: boolean): void;
    /**
     * IME composition started: the pending text replaces the selection.
     */
    composition_start(): void;
    /**
     * Preedit text changed. It lives in the backing string so it shapes and
     * renders inline (underlined) at the caret.
     */
    composition_update(text: string): void;
    /**
     * Insert text at the caret, replacing the selection.
     */
    insert_text(text: string): void;
    /**
     * Handle a `keydown`. `key` is the raw `KeyboardEvent.key` value; any
     * single-character key inserts itself. Returns true when the event was
     * consumed and JS should call `preventDefault()`.
     */
    key_input(key: string, ctrl: boolean, shift: boolean): boolean;
    /**
     * Pointer coordinates are CSS px relative to the canvas.
     */
    pointer_down(x: number, y: number): void;
    pointer_move(x: number, y: number): void;
    pointer_up(): void;
    /**
     * Draw a frame, unless nothing changed since the last one. Returns whether
     * anything was actually rendered and presented — an idle demo renders
     * zero frames per second and the canvas still shows the right thing.
     */
    render(): boolean;
    resize(width: number, height: number, dpr: number): void;
    /**
     * The currently selected text (for clipboard integration in JS).
     */
    selected_text(): string;
    /**
     * Caret blink is host-side: JS toggles this on a timer and resets it on
     * every edit or motion.
     */
    set_caret_visible(visible: boolean): void;
    set_font_size(size: number): void;
    set_search(needle: string): void;
    /**
     * How search matches are marked: `highlight` (an alpha-blended rect over
     * the glyphs), or `underline`, `squiggle` or `chip` — the same cursor
     * ranges routed through the decoration pipeline instead.
     */
    set_search_mode(mode: string): void;
    /**
     * Swap the editable pane for a live terminal grid: a synthetic colored
     * log streams into a [`TermGrid`] sized to the canvas, drawn out of one
     * retained block with the mono face and procedural box drawing.
     *
     * The editable blocks are hidden, never destroyed, and no edit is
     * accepted while this is on — so toggling it back off restores the text,
     * the caret and the selection exactly as they were.
     */
    set_terminal(on: boolean): void;
    set_text(text: string): void;
    /**
     * Stand the text pane up in 3D: `degrees` of rotation about its own
     * vertical axis, seen through a perspective camera. `undefined` puts it
     * back flat, where the renderer's placement math is bit-for-bit the plain
     * 2D path again.
     *
     * Nothing re-shapes and no instance is rebuilt — this is one matrix per
     * block per frame — so a slow sway is free to animate. Pointer selection
     * keeps working: [`FafTextDemo::pointer_down`] casts a ray instead of
     * reading a pixel.
     */
    set_tilt(degrees?: number | null): void;
    /**
     * Blend the whole view between the variable font's lightest and boldest
     * masters, in the fragment shader: 0 is the axis minimum, 1 the maximum.
     * `undefined` goes back to the static demo font and its shaped weight.
     * Cheap enough to drive from a per-frame sine — nothing re-shapes and no
     * curve is re-extracted.
     */
    set_weight_blend(t?: number | null): void;
    /**
     * Whether terminal mode is on.
     */
    terminal(): boolean;
    /**
     * The full text, so JS can round-trip it (save, copy-all, tests).
     */
    text(): string;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_faftextdemo_free: (a: number, b: number) => void;
    readonly faftextdemo_attach: (a: any, b: number, c: number) => any;
    readonly faftextdemo_backend: (a: number) => [number, number];
    readonly faftextdemo_caret_css_rect: (a: number) => [number, number];
    readonly faftextdemo_composition_end: (a: number, b: number) => void;
    readonly faftextdemo_composition_start: (a: number) => void;
    readonly faftextdemo_composition_update: (a: number, b: number, c: number) => void;
    readonly faftextdemo_insert_text: (a: number, b: number, c: number) => void;
    readonly faftextdemo_key_input: (a: number, b: number, c: number, d: number, e: number) => number;
    readonly faftextdemo_pointer_down: (a: number, b: number, c: number) => void;
    readonly faftextdemo_pointer_move: (a: number, b: number, c: number) => void;
    readonly faftextdemo_pointer_up: (a: number) => void;
    readonly faftextdemo_render: (a: number) => [number, number, number];
    readonly faftextdemo_resize: (a: number, b: number, c: number, d: number) => void;
    readonly faftextdemo_selected_text: (a: number) => [number, number];
    readonly faftextdemo_set_caret_visible: (a: number, b: number) => void;
    readonly faftextdemo_set_font_size: (a: number, b: number) => void;
    readonly faftextdemo_set_search: (a: number, b: number, c: number) => void;
    readonly faftextdemo_set_search_mode: (a: number, b: number, c: number) => void;
    readonly faftextdemo_set_terminal: (a: number, b: number) => void;
    readonly faftextdemo_set_text: (a: number, b: number, c: number) => void;
    readonly faftextdemo_set_tilt: (a: number, b: number) => void;
    readonly faftextdemo_set_weight_blend: (a: number, b: number) => void;
    readonly faftextdemo_terminal: (a: number) => number;
    readonly faftextdemo_text: (a: number) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h6eb6ae39273879d5: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h2cdb68df6893a714: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h2cdb68df6893a714_2: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h2cdb68df6893a714_3: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h97af8ec3bf76bae8: (a: number, b: number, c: any, d: any) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
