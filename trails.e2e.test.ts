import type { NodeMap } from './types';
import * as testUtils from './test-utils';
import * as lib from './lib';
import * as trails from './trails';

test('trails', async () => {
	const map: NodeMap = testUtils.buildIndexCached();
	const trailheads: string[] = testUtils.getTrailheadsCached();

	const batLakeTH = trailheads.find(th => map[th].nodes.find(x => x.name === 'Bat Lake Trail'));
	expect(batLakeTH).toBeTruthy();
	console.log(batLakeTH);

	const allTrails = trails.getTrails(map, batLakeTH!);

	const dedupedTrails = trails.groupAndFilter(allTrails);
	expect(dedupedTrails.length).toBe(1);
	expect(dedupedTrails[0].coords.length).toBe(155);
});
