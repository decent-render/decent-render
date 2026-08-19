import {useCurrentFrame, useVideoConfig} from 'remotion';

/**
 * CPU/DOM-bound benchmark: no GPU work, no WebGL/WebGPU context. Layout and
 * paint cost scales with BOX_COUNT. Deliberately deterministic — every frame is
 * a pure function of `frame`, so two renders of the same frame range must
 * produce identical output (the property §9's determinism harness checks).
 */
const BOX_COUNT = 400;

export const Standard: React.FC = () => {
	const frame = useCurrentFrame();
	const {width, height} = useVideoConfig();

	return (
		<div style={{width, height, background: '#0b0b0f', position: 'relative'}}>
			{Array.from({length: BOX_COUNT}, (_, i) => {
				const phase = (i / BOX_COUNT) * Math.PI * 2;
				const t = frame / 30 + phase;
				const size = 20 + ((i * 7) % 40);
				return (
					<div
						key={i}
						style={{
							position: 'absolute',
							left: (Math.cos(t) * 0.4 + 0.5) * (width - size),
							top: (Math.sin(t * 1.3) * 0.4 + 0.5) * (height - size),
							width: size,
							height: size,
							borderRadius: 4,
							background: `hsl(${(i * 137.5) % 360} 70% 55%)`,
							transform: `rotate(${(frame * 2 + i) % 360}deg)`,
							opacity: 0.85,
						}}
					/>
				);
			})}
		</div>
	);
};
