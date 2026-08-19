import {useCallback, useEffect, useRef, useState} from 'react';
import {continueRender, delayRender, useCurrentFrame, useVideoConfig} from 'remotion';

/**
 * GPU-bound benchmark: a full-screen WGSL fragment shader whose per-pixel cost
 * scales with ITERATIONS. Uses raw WebGPU (no three.js) so the measurement is of
 * the browser's GPU path itself, which is the thing the farm exists to provide.
 *
 * If WebGPU is unavailable the component throws rather than silently falling
 * back — a benchmark that quietly measures a software rasterizer is worse than
 * no benchmark. `decent bench` reports that as "gpu unavailable".
 */
const ITERATIONS = 220;

const SHADER = /* wgsl */ `
struct Uniforms { time: f32, iterations: f32 };
@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
  var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  return vec4f(p[i], 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
  let uv = (pos.xy / 512.0 - vec2f(1.0)) * 1.6;
  var z = vec2f(0.0);
  let c = uv + vec2f(cos(u.time) * 0.15, sin(u.time) * 0.15);
  var hits = 0.0;
  let n = i32(u.iterations);
  for (var k = 0; k < n; k = k + 1) {
    z = vec2f(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) + c;
    if (dot(z, z) > 4.0) { break; }
    hits = hits + 1.0;
  }
  let m = hits / u.iterations;
  return vec4f(m, m * 0.4 + 0.2 * sin(u.time), 1.0 - m, 1.0);
}
`;

export const WebGPUScene: React.FC = () => {
	const canvasRef = useRef<HTMLCanvasElement>(null);
	const frame = useCurrentFrame();
	const {width, height, fps} = useVideoConfig();
	const [handle] = useState(() => delayRender('webgpu frame'));
	const gpuRef = useRef<{
		device: GPUDevice;
		context: GPUCanvasContext;
		pipeline: GPURenderPipeline;
		uniform: GPUBuffer;
		bindGroup: GPUBindGroup;
	} | null>(null);

	const draw = useCallback(async () => {
		const canvas = canvasRef.current;
		if (!canvas) return;

		if (!gpuRef.current) {
			if (!navigator.gpu) throw new Error('WebGPU unavailable: navigator.gpu is undefined');
			const adapter = await navigator.gpu.requestAdapter();
			if (!adapter) throw new Error('WebGPU unavailable: no adapter');
			const device = await adapter.requestDevice();
			const context = canvas.getContext('webgpu') as GPUCanvasContext | null;
			if (!context) throw new Error('WebGPU unavailable: no webgpu canvas context');
			const format = navigator.gpu.getPreferredCanvasFormat();
			context.configure({device, format, alphaMode: 'opaque'});
			const module = device.createShaderModule({code: SHADER});
			const pipeline = device.createRenderPipeline({
				layout: 'auto',
				vertex: {module, entryPoint: 'vs'},
				fragment: {module, entryPoint: 'fs', targets: [{format}]},
				primitive: {topology: 'triangle-list'},
			});
			const uniform = device.createBuffer({size: 8, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST});
			const bindGroup = device.createBindGroup({
				layout: pipeline.getBindGroupLayout(0),
				entries: [{binding: 0, resource: {buffer: uniform}}],
			});
			gpuRef.current = {device, context, pipeline, uniform, bindGroup};
		}

		const {device, context, pipeline, uniform, bindGroup} = gpuRef.current;
		device.queue.writeBuffer(uniform, 0, new Float32Array([frame / fps, ITERATIONS]));
		const encoder = device.createCommandEncoder();
		const pass = encoder.beginRenderPass({
			colorAttachments: [
				{
					view: context.getCurrentTexture().createView(),
					clearValue: {r: 0, g: 0, b: 0, a: 1},
					loadOp: 'clear',
					storeOp: 'store',
				},
			],
		});
		pass.setPipeline(pipeline);
		pass.setBindGroup(0, bindGroup);
		pass.draw(3);
		pass.end();
		device.queue.submit([encoder.finish()]);
		// Block until the GPU has actually finished this frame, otherwise the
		// screenshot can capture work that hasn't landed yet.
		await device.queue.onSubmittedWorkDone();
	}, [frame, fps]);

	useEffect(() => {
		draw()
			.then(() => continueRender(handle))
			.catch((err) => {
				throw err;
			});
	}, [draw, handle]);

	return <canvas ref={canvasRef} width={width} height={height} style={{width, height}} />;
};
