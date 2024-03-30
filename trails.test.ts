import type { NodeMap } from './types';
import * as testUtils from './test-utils';

test('trails', async () => {
	const map: NodeMap = testUtils.buildIndexCached();
	const trailheads: string[] = testUtils.getTrailheadsCached();

	const batLakeTH = trailheads.find(th => map[th].nodes.find(x => x.name === 'Bat Lake Trail'));
	expect(batLakeTH).toBeTruthy();
	console.log(batLakeTH);
});
