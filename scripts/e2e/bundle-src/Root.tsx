import {Composition} from 'remotion';
import React from 'react';

export const P3Comp: React.FC<{frame: number}> = ({frame}) => {
	// Trivial but real: a solid colour sweep + frame counter, 30 frames.
	const g = Math.round((frame / 30) * 255);
	return (
		<div style={{flex: 1, backgroundColor: `rgb(40, ${g}, 120)`, display: 'flex', alignItems: 'center', justifyContent: 'center'}}>
			<span style={{color: 'white', fontFamily: 'Helvetica', fontSize: 64}}>{`p3 ${frame}`}</span>
		</div>
	);
};

export const RemotionRoot: React.FC = () => (
	<Composition id="p3comp" component={P3Comp} durationInFrames={30} fps={30} width={640} height={360} defaultProps={{frame: 0}} />
);
