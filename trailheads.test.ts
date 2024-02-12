import type { NodeMap } from './types';
import * as lib from './lib';

const features = lib.load('../basemap/ztrails.geojsonl');
const map: NodeMap = lib.buildIndex(features);

test('trailheads seem reasonable', async () => {
});
