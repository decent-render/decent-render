import {Composition} from 'remotion';
import {Standard} from './Standard';
import {WebGPUScene} from './WebGPUScene';

/**
 * The golden benchmark suite (§9). Duration and size come from input props via
 * calculateMetadata, so one composition covers short and long variants without
 * rebundling — the harness varies the job, not the bundle.
 */
type Props = {durationInFrames?: number; width?: number; height?: number};

const metadata = ({props}: {props: Props}) => ({
	durationInFrames: props.durationInFrames ?? 150,
	width: props.width ?? 1920,
	height: props.height ?? 1080,
});

export const RemotionRoot: React.FC = () => (
	<>
		<Composition
			id="Standard"
			component={Standard}
			durationInFrames={150}
			fps={30}
			width={1920}
			height={1080}
			calculateMetadata={metadata}
		/>
		<Composition
			id="WebGPU"
			component={WebGPUScene}
			durationInFrames={150}
			fps={30}
			width={1920}
			height={1080}
			calculateMetadata={metadata}
		/>
	</>
);
